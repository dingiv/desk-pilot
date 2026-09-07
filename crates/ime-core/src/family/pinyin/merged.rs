//! merged — MergedDict:三层词典合并视图(round17 建,round19 三级化)。
//!
//! 多词典体系(磁盘 / 内存):
//!
//! ```text
//! 磁盘  SeedDict(rime-ice.fst,系统自带,不可变)
//!       OverlayDict(SQLite overlay_freq/overlay_recent 两表快照)
//!       Wordbook(运行时实时同步的自生词:词 + 拼音 + 词频)
//! 内存  SeedDict / OverlayDict(启动冷加载)
//!       OverlayData(MemoryLayer,L1 工作集,攒满 flush 进 L2)
//!       MergedDict = 三层合并视图(本模块)
//! ```
//!
//! **同一预测,对三个词典各查一次**(round19 指令:三层采用完全相同的
//! 查询方式 —— 全拼精确命中 → (词, 频率)):
//!
//! - L3 SeedDict:现有 FST/lattice 查询路径(不变);
//! - L2 OverlayDict:`lookup(input)` 独立出候选(自生词即使不在 seed
//!   也能出现),`freq_to_score(有效频率)` 同刻度换算;
//! - L1 OverlayData:`lookup_pinyin` 精确命中,同上(量小,工作集)。
//!
//! **权重覆盖(越热越权威)**:同一预测选项多层命中时,以最高优先层的
//! 有效频率为准 —— `L1 > L2 > L3`。统计微调(recency tier,三级穿透)
//! 在覆盖之后照旧叠加:overlay 定基线,统计做微调。

use super::lattice::LatticeDecoder;
use crate::family::scoring::FreqScale;
use crate::family::ScoredCandidate;
use crate::store::memory::MemoryLayer;
use crate::store::overlay_dict::OverlayDict;

/// 注入候选上限(L1+L2 出候选的去重池;量小,避免挤占种子候选页)。
const OVERLAY_CAND_CAP: usize = 8;

/// 三层覆盖 + 独立出候选。
///
/// - **覆盖**:候选词 L1 命中 → L1 有效频率;否则 L2 命中 → L2 有效
///   频率;否则保持种子分。要求拼音映射对 == 当前输入(同词不同音不算
///   同一预测选项)、频率 > 0(旧迁移种子不覆盖)。
/// - **出候选**:L1/L2 中 pinyin == input 但种子路径没出的词(自生词)
///   以 `source = "overlay"` 注入,`freq_to_score(有效频率)` 定分。
pub fn apply_overlay_override(
    mem: &MemoryLayer,
    l2: &OverlayDict,
    input: &str,
    lattice: Option<&LatticeDecoder>,
    freq_scale: &FreqScale,
    cands: &mut Vec<ScoredCandidate>,
) {
    let Some(lat) = lattice else {
        return;
    };

    // ── 覆盖:逐候选取最高优先层的有效频率 ──────────────────────────
    for c in cands.iter_mut() {
        if let Some(e) = mem.freq_entry(&c.text) {
            if e.frequency > 0 && !e.pinyin.is_empty() && e.pinyin == input {
                c.raw_score = lat.freq_to_score(freq_scale, e.effective_frequency());
                continue; // L1 最热,直接定分
            }
        }
        if let Some(e) = l2.freq_entry(&c.text) {
            if e.frequency > 0 && !e.pinyin.is_empty() && e.pinyin == input {
                c.raw_score = lat.freq_to_score(freq_scale, e.effective_frequency());
            }
        }
    }

    // ── 独立出候选:L1/L2 有、种子路径没出的词(自生词主路)──────────
    let mut present: std::collections::HashSet<String> =
        cands.iter().map(|c| c.text.clone()).collect();
    let mut injected = 0;
    type CandSet = std::collections::HashSet<String>;
    let push_overlay = |text: &str, freq: u64,
                            present: &mut CandSet,
                            injected: &mut usize,
                            cands: &mut Vec<ScoredCandidate>| {
        if *injected >= OVERLAY_CAND_CAP || freq == 0 || present.contains(text) {
            return;
        }
        *injected += 1;
        present.insert(text.to_string());
        cands.push(ScoredCandidate {
            text: text.to_string(),
            family: "pinyin",
            source: "overlay",
            raw_score: lat.freq_to_score(freq_scale, freq),
        });
    };
    for (w, e) in mem.lookup_pinyin(input) {
        if e.pinyin == input {
            push_overlay(w, e.effective_frequency(), &mut present, &mut injected, cands);
        }
    }
    for (w, f) in l2.lookup(input) {
        push_overlay(&w, f, &mut present, &mut injected, cands);
    }
}
