//! wordbook — 单词本独立模块(round14)。
//!
//! **所有权**:持久化模块(`store::PersistenceManager`)持有 `WordBook`
//! 本体(Arc),引擎装配时分发引用克隆。依赖方向:
//!
//! ```text
//! PersistenceManager(所有) ── Arc<WordBook> ──┬─ SessionState(引用)
//!                                             ├─ PinyinFamily(依赖)
//!                                             └─ EnglishFamily(依赖)
//! ```
//!
//! - **家族依赖单词本**:预测时读自身册(自生词短语 / user 层 / recency
//!   加成)—— 家族内部状态 + 外部 context → 预测,单词本是外部注入的
//!   共享存储。
//! - **后处理依赖单词本**:`post::learn_commit` 学习结算直接写册
//!   (recency / 自生词 / 英文自生词),家族不感知 commit。
//! - **L0 边界**:拼音 L0 用户模型(pins/pick counters)实现在
//!   `inputx_pinyin` 引擎词典内部,不随本轮外搬 —— 经家族句柄
//!   `record_pick` 写入(见 post.rs PinyinPick 分支),持久化走
//!   `store.save_l0`。
//!
//! 写穿(SQLite)由各册内置的 `store` 槽承担:`set_store` 在
//! `init_store` 时注入,学习/提交路径双写与旧实现同拍。

use crate::family::pinyin::phrase::PhraseBook;
use crate::store::WeightStore;
use std::sync::{Arc, Mutex};

/// 单词本 + 统一记忆层(跨会话共享;持久化模块所有)。
///
/// round16:`recency` 从两册摘除,统一进 [`MemoryLayer`](overlay dict:
/// 词 → (拼音映射对, 最近提交时间, 累计提交次数));家族/后处理经
/// `wordbook.memory` 共享。
#[derive(Default)]
pub struct WordBook {
    pub pinyin: PinyinBook,
    pub english: EnglishBook,
    /// L1 OverlayData(round16):实时工作集,攒满即 flush 进 L2。
    /// (Mutex:`Arc<WordBook>` 共享下的内部可变性。)
    pub memory: Mutex<crate::store::memory::MemoryLayer>,
    /// L2 OverlayDict(round19 三级架构):持久化沉淀层,冷加载 +
    /// flush 时吸收 L1;独立预测层(与 SeedDict 同一查询方式)。
    pub overlay_dict: Mutex<crate::store::overlay_dict::OverlayDict>,
}

impl WordBook {
    /// 提交登记到统一记忆层(时间盖章 + 计数;拼音侧带 SQLite 快照双写,
    /// 英文侧进程内生命周期 —— 与旧 recency 双册行为严格一致)。
    pub fn record_commit(&self, english: bool, word: &str) {
        if english {
            self.memory
                .lock()
                .unwrap()
                .record_commit(word, "", crate::family::now_ms());
        } else {
            let mut mem = self.memory.lock().unwrap();
            mem.record_commit(word, "", crate::family::now_ms());
            drop(mem);
            self.maybe_flush();
        }
    }

    /// 自生词登记进统一记忆层(overlay dict;count=0,提交后增长)。
    pub fn register_memory(&self, pinyin: &str, word: &str) {
        self.memory
            .lock()
            .unwrap()
            .register_self_generated(word, pinyin);
    }

    /// flush 检查(round19):L1 攒满阈值 → 搬入 L2 并全量快照落盘
    /// (L2 两表:overlay_freq / overlay_recent —— 分表)。
    /// 返回搬走的词数(0 = 未达阈值或无持久化句柄)。
    pub fn maybe_flush(&self) -> usize {
        let reached = {
            let mem = self.memory.lock().unwrap();
            mem.len() >= crate::store::memory::FLUSH_THRESHOLD
        };
        if !reached {
            return 0;
        }
        let mut l2 = self.overlay_dict.lock().unwrap();
        let n = self.memory.lock().unwrap().flush_into(&mut l2);
        if n > 0 {
            if let Some(ref store) = *self.pinyin.store.lock().unwrap() {
                let snap = l2.dump();
                store.save_overlay_freq(&snap.freq);
                store.save_overlay_recent(&snap.recent);
            }
        }
        n
    }

    /// 无条件 flush(round19:引擎关闭时保底,未达阈值的 L1 不丢)。
    pub fn flush_now(&self) -> usize {
        {
            let mem = self.memory.lock().unwrap();
            if mem.is_empty() {
                return 0;
            }
        }
        let mut l2 = self.overlay_dict.lock().unwrap();
        let n = self.memory.lock().unwrap().flush_into(&mut l2);
        if n > 0 {
            if let Some(ref store) = *self.pinyin.store.lock().unwrap() {
                let snap = l2.dump();
                store.save_overlay_freq(&snap.freq);
                store.save_overlay_recent(&snap.recent);
            }
        }
        n
    }

    /// 手工频率调整(round22 ④,#freq/up|down):一笔 ±FREQ_MANUAL_STEP
    /// 记入 L1 增量账本;`seed_base` 在词条尚无基础频率时补继承。
    /// 返回调整前后的有效频率(未接线/空词 → None)。
    pub fn adjust_freq(
        &self,
        pinyin: &str,
        word: &str,
        step: i64,
        seed_base: Option<u64>,
    ) -> Option<(u64, u64)> {
        if word.is_empty() {
            return None;
        }
        let mut m = self.memory.lock().unwrap();
        let before = m.freq_entry(word).map(|e| e.effective_frequency()).unwrap_or(0);
        m.apply_manual_adjust(word, pinyin, step, seed_base);
        let after = m.freq_entry(word).map(|e| e.effective_frequency()).unwrap_or(0);
        Some((before, after))
    }

    /// 近期增益三级穿透(round21):L1 时间表 miss → L2 时间表
    /// (flush 清空 L1 后,刚沉淀的词不丢近期加成)。返回值直接就是
    /// 合成系数:`score' = a + (1-a) × g`(g ∈ [0, GAIN_MAX])。
    pub fn recency_boost(&self, word: &str, now_ms: i64) -> f64 {
        let mut l1 = self.memory.lock().unwrap();
        let g = l1.recency_boost(word, now_ms);
        if g > 0.0 {
            return g;
        }
        drop(l1);
        self.overlay_dict.lock().unwrap().recency_boost(word, now_ms)
    }

    /// 手工增量查询(round24,#freq 拉黑/加权消费侧):L1 → L2 三级穿透,
    /// 要求拼音映射对 == 查询拼音(同预测选项语义)。增量仅在
    /// `#freq/up|down` 产生:负 = 用户显式降权,正 = 显式加权(也可能含
    /// 有机增量 —— 有机永不产生负笔,负号即拉黑信号)。
    pub fn manual_delta(&self, pinyin: &str, word: &str) -> Option<i64> {
        if let Some(e) = self.memory.lock().unwrap().freq_entry(word) {
            if !e.pinyin.is_empty() && e.pinyin == pinyin {
                return Some(e.delta);
            }
        }
        let l2 = self.overlay_dict.lock().unwrap();
        let e = l2.freq_entry(word)?;
        (!e.pinyin.is_empty() && e.pinyin == pinyin).then_some(e.delta)
    }

    /// 某拼音输入下被拉黑(delta < 0)的词条,|delta| 降序(round24):
    /// L1 ∪ L2(#freq/up 恢复回退 —— 被拉黑词已沉出候选页、高亮不到,
    /// up 的对象按“恢复我拉黑过的词”回退定位)。
    pub fn downweighted_for(&self, pinyin: &str) -> Vec<(String, i64)> {
        let mut out: Vec<(String, i64)> = self
            .memory
            .lock()
            .unwrap()
            .downweighted(pinyin);
        let l2 = self.overlay_dict.lock().unwrap();
        for (w, d) in l2.downweighted(pinyin) {
            if !out.iter().any(|(rw, _)| rw == &w) {
                out.push((w, d));
            }
        }
        out.sort_by_key(|(_, d)| -d.abs());
        out.dedup();
        out
    }
}

/// 拼音册:自生词短语本(recency 已归 [`WordBook::memory`])。
#[derive(Default)]
pub struct PinyinBook {
    pub(crate) phrase_book: Mutex<PhraseBook>,
    pub(crate) store: Mutex<Option<Arc<WeightStore>>>,
}

impl PinyinBook {
    /// 注入持久化句柄(`init_store` 时)。
    pub fn set_store(&self, store: Arc<WeightStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    fn store(&self) -> Option<Arc<WeightStore>> {
        self.store.lock().unwrap().clone()
    }

    /// 自生词入本(无条件路径:多字拼音逐字选择组成的整体)。
    /// 已在本内 → bump 使用计数;否则插入。双写 SQLite。
    pub fn learn_composed_phrase(&self, pinyin: &str, hanzi: &str) {
        let mut book = self.phrase_book.lock().unwrap();
        if book.count(pinyin, hanzi) > 0 {
            book.bump_count(pinyin, hanzi);
            if let Some(ref store) = self.store() {
                store.bump_phrase_count(pinyin, hanzi);
            }
        } else {
            book.insert(pinyin, hanzi);
            if let Some(ref store) = self.store() {
                store.record_phrase(pinyin, hanzi, 0);
            }
        }
    }



}

/// 英文册:user 层自生词(recency 已归 [`WordBook::memory`])。
#[derive(Default)]
pub struct EnglishBook {
    pub(crate) user_words: Mutex<Vec<(String, u32)>>,
    pub(crate) store: Mutex<Option<Arc<WeightStore>>>,
}

impl EnglishBook {
    /// 注入持久化句柄(`init_store` 时)。
    pub fn set_store(&self, store: Arc<WeightStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    fn store(&self) -> Option<Arc<WeightStore>> {
        self.store.lock().unwrap().clone()
    }

    /// 英文自生词入本(Enter 强选 raw 纯 ASCII 词;权重 10000 压过
    /// emoji 前缀与中文简拼)+ SQLite en_user 双写。
    /// 合并语义与历史一致:小写键去重,**权重大者胜、后见的大小写胜出**。
    pub fn learn_word(&self, word: &str) {
        self.merge_user(&[(word.to_string(), 10_000)]);
        if let Some(ref store) = self.store() {
            store.record_en_user(word);
        }
    }

    /// user 层合并(小写键去重;权重大者胜、后见的大小写胜出)后按
    /// 大小写不敏感排序。
    pub fn merge_user(&self, words: &[(String, u32)]) {
        let mut user = self.user_words.lock().unwrap();
        let mut merged: std::collections::HashMap<String, (String, u32)> = user
            .iter()
            .map(|(w, s)| (w.to_ascii_lowercase(), (w.clone(), *s)))
            .collect();
        for (w, s) in words {
            let key = w.to_ascii_lowercase();
            let entry = merged.entry(key).or_insert_with(|| (w.clone(), 0));
            if *s >= entry.1 {
                entry.0 = w.clone();
                entry.1 = *s;
            }
        }
        *user = merged.into_values().collect();
        user.sort_by_key(|(w, _)| w.to_lowercase());
    }


}
