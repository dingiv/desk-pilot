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
use crate::family::pinyin::recency::RecentStore;
use crate::store::WeightStore;
use std::sync::{Arc, Mutex};

/// 单词本(跨会话共享;持久化模块所有)。
#[derive(Default)]
pub struct WordBook {
    pub pinyin: PinyinBook,
    pub english: EnglishBook,
}

/// 拼音册:自生词短语本 + 最近使用(recency)。
#[derive(Default)]
pub struct PinyinBook {
    pub(crate) phrase_book: Mutex<PhraseBook>,
    pub(crate) recency: Mutex<RecentStore>,
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

    /// 提交 recency:盖章 + SQLite 全量快照双写(时间衰减跨重启存活)。
    pub fn record_commit(&self, word: &str) {
        let mut rec = self.recency.lock().unwrap();
        rec.record(word, crate::family::now_ms());
        if let Some(ref store) = self.store() {
            store.save_recency(&rec.dump());
        }
    }
}

/// 英文册:user 层自生词 + 最近使用(recency)。
#[derive(Default)]
pub struct EnglishBook {
    pub(crate) user_words: Mutex<Vec<(String, u32)>>,
    pub(crate) recency: Mutex<RecentStore>,
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

    /// 提交 recency 加权(进程内生命周期,不持久化)。
    pub fn record_commit(&self, word: &str) {
        self.recency.lock().unwrap().record(word, crate::family::now_ms());
    }
}
