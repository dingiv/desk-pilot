//! Candidate family system — pluggable prediction sources with unified
//! weight-based ranking.
//!
//! ## Architecture
//!
//! Each [`CandidateFamily`] independently generates scored candidates from
//! user input. The [`UnifiedScorer`] collects candidates from all enabled
//! families, applies inter-family priority weighting, deduplicates, and
//! returns a globally ranked list.
//!
//! **Only pinyin + english participate in the unified ranking** (中英混输
//! competition). Magic commands (`#`) and snippets (`/`) have mandatory
//! trigger prefixes: the FSM routes them into their own composition state and
//! fills candidates directly from the trie/registry — they never pass through
//! the scorer.
//!
//! ```text
//! Input → PinyinFamily (priority=100) → [候选A:0.95, 候选B:0.80, ...]  (含 jianpin 组合)
//!       → EnglishFamily (priority=70)  → [black:0.88, ...]
//!                    ↓
//!       UnifiedScorer::rank()
//!         → final_score = raw_score × (priority / 100)
//!         → sort desc, dedup
//!                    ↓
//!            [最终排序列表]
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// ── ScoredCandidate ─────────────────────────────────────────────────────

/// A candidate word with its family origin and internal score.
#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    /// The candidate text (hanzi, English word, snippet expansion, etc.).
    pub text: String,
    /// Which family produced this candidate.
    pub family: &'static str,
    /// Which member within the family produced this candidate
    /// (e.g., "dict", "viterbi", "session", "phrase", "jianpin", "prefix").
    pub source: &'static str,
    /// Family-internal score in [0.0, 1.0]. Higher = better match.
    pub raw_score: f64,
}

/// Stage 2 收集产物:单家族的预过滤候选(raw 分降序)+ 该家族的
/// priority 乘数。合成(×bonus/全局排序/去重)在 stage3 的
/// [`UnifiedScorer::merge`]。
#[derive(Debug)]
pub struct FamilyCandidates {
    pub bonus: f64,
    pub candidates: Vec<ScoredCandidate>,
}

/// A final-ranked candidate after global scoring and dedup.
/// Returned by [`UnifiedScorer::rank_detailed`].
#[derive(Debug, Clone)]
pub struct RankedCandidate {
    pub text: String,
    /// Final score after family priority weighting.
    pub score: f64,
    /// Which family ("pinyin", "english", ...).
    pub family: &'static str,
    /// Which member within the family ("dict", "viterbi", "session", ...).
    pub source: &'static str,
}

// ── InputContext ────────────────────────────────────────────────────────

/// Short-term input context — what the user recently committed.
/// Passed to every family's `predict` so they can adjust rankings
/// based on what came before.
///
/// **状态标注(D1,2026-08-29)**:管道已铺好但**当前无家族消费** ——
/// pinyin 的上下文来源是自身的 `last_commit`(record_pick 写入,含
/// 拼音键,信息量比 `last_word` 大),english/emoji 未覆盖
/// `predict_with_context`。保留原因:上下文感知是后续重点
/// (english recency、跨 turn 联想都计划走这条管道),届时家族改为
/// 消费 ctx,`last_commit` 退役。
#[derive(Debug, Clone, Default)]
pub struct InputContext {
    /// Last few committed characters (up to ~20 chars).
    pub recent_text: String,
    /// Last committed word (single word boundary).
    pub last_word: String,
}

impl InputContext {
    pub fn new() -> Self {
        InputContext::default()
    }

    pub fn update(&mut self, text: &str) {
        self.last_word = text.to_string();
        self.recent_text.push_str(text);
        // Keep only the last 20 characters.
        if self.recent_text.chars().count() > 20 {
            let skip = self.recent_text.chars().count() - 20;
            self.recent_text = self.recent_text.chars().skip(skip).collect();
        }
    }
}

/// 当前 wall-clock 毫秒(unix epoch)—— recency 的时间基准(拼音/英文共享)。
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

// ── FamilyEnv:家族侧的环境能力接面 ─────────────────────────────────────
//
// 依赖单向化(R4):本 trait 由 **family 定义**,fsm 侧的 `StepEnv`
// 继承它(fsm → family 单向)。家族代码(member predict / query)只看
// 得到 FamilyEnv —— 家族需要什么,家族自己说了算。

/// 家族预测/成员会话所需的环境能力(由引擎提供实现)。
pub trait FamilyEnv: Send + Sync {
    /// 片段变量展开器(`$DATE` / `$CLIPBOARD` / `$CURSOR`)。
    fn expander(&self) -> &crate::family::magic::expander::Expander;

    /// voice server 命令 sender(`#asr` Attach/Detach、分字符归档)。
    /// 未接线(测试/无 aura)时 `None`。
    fn voice_cmd_tx(&self) -> Option<crate::io_thread::VoiceCmdSender> {
        None
    }

    /// 拼音家族:L0 选择记录(频率加成)。
    fn record_pick(&self, pinyin: &str, word: &str) {
        let _ = (pinyin, word);
    }
    /// 学短语(已在词典的词不入本)。
    fn learn_phrase(&self, _pinyin: &str, _hanzi: &str) {}
    /// 造词学习(逐字选择完成后整词入单词本)。
    fn learn_composed_phrase(&self, _pinyin: &str, _hanzi: &str) {}
    /// 提交落地学习:recency/bigram 按提交家族分派(`Some("english")` →
    /// 英文家族,其余 → 拼音家族)。
    fn record_commit_text(&self, _word: &str, _family: Option<&str>) {}
    /// `#del` 的 del_len 选项用:记录本次提交的字符数。
    fn record_commit_len(&self, _word: &str) {}
    /// 纯 ASCII 字母数字提交学入英文家族 user 层(自生词;英文候选提交不学)。
    fn learn_ascii_word(&self, _word: &str) {}
    /// 造词单字候选(首音节词典序;链式/单音节返回空)。
    fn compose_single_chars(
        &self,
        _input: &str,
        _ctx: &InputContext,
        _existing: &[String],
        _limit: usize,
    ) -> Vec<ScoredCandidate> {
        Vec::new()
    }
}

// ── CandidateFamily trait ───────────────────────────────────────────────

/// A pluggable prediction source. Each family is an independent engine
/// that generates candidates from the raw input buffer.
pub trait CandidateFamily: Send + Sync {
    /// Unique family identifier (e.g., "pinyin", "jianpin").
    fn name(&self) -> &'static str;

    /// Inter-family priority (0–100). Higher-priority families get a
    /// larger multiplier in the final ranking.
    fn priority(&self) -> u32;

    /// Whether this family is currently active. Disabled families are
    /// skipped entirely by the [`UnifiedScorer`].
    fn enabled(&self) -> bool {
        true
    }

    /// 运行时启/禁家族(默认 no-op)。EmojiFamily 覆盖:
    /// `dicts.emoji: false` 时整个家族退出统一打分。
    fn set_family_enabled(&self, _on: bool) {}

    /// 跨家族竞争宽度 —— 家族候选进入统一排序的**语义截断唯一入口**
    /// (家族内部的 take/truncate 是引擎预过滤,不是语义截断)。
    fn top_n(&self) -> usize {
        8
    }

    /// Generate scored candidates from the raw input buffer.
    /// 单入口(S1 规范化):上下文经 `ctx` 传入,家族内部自决如何消费
    /// (gate 开关、last_commit/bigram/recency 全部封装在家族内,调用方
    /// 无需感知家族差异)。无上下文的调用点传 `&InputContext::new()`。
    fn predict(&self, input: &str, ctx: &InputContext) -> Vec<ScoredCandidate>;

    /// Load an external dictionary file into this family's vocabulary.
    fn load_dict(&self, _path: &str) -> std::io::Result<usize> {
        Ok(0)
    }

    /// Load a user dictionary (all entries get maximum priority).
    fn load_user_dict(&self, _path: &str) -> std::io::Result<usize> {
        Ok(0)
    }

    /// Load dictionary entries from raw TSV bytes (for embedded dicts).
    fn load_dict_bytes(&self, _data: &[u8]) -> usize {
        0
    }

    /// Attach the weight store for persisting learned data.
    /// Called once at startup after init_store.
    fn attach_store(&self, _store: std::sync::Arc<crate::store::WeightStore>) {}
}

// 家族私有能力(学习/暖启/上下文开关)**不在 trait 上** —— D5 接口隔离:
//   pinyin:record_pick / learn_phrase / learn_composed_phrase /
//          export_l0_json / import_l0_json / set_context_aware /
//          record_commit / warm_recencies / warm_phrases_from_store
//   english:record_learned_word / warm_learned_words
// 经引擎直持的具体句柄(Arc<PinyinFamily> /
// Arc<EnglishFamily>)直接调用,不再走 trait 对象的 no-op 默认实现。

// ── UnifiedScorer ───────────────────────────────────────────────────────

/// Collects candidates from all enabled families and returns a globally
/// ranked, deduplicated list.
pub struct UnifiedScorer {
    families: Vec<Box<dyn CandidateFamily>>,
    /// Configurable per-family priority overrides (from `swift-ime.yaml`);
    /// families without an entry keep their own `priority()`.
    priorities: FamilyPriorities,
    /// Per-family `top_n` overrides(round10:`weights.family_top_n`)——
    /// 跨家族竞争宽度的配置入口;无覆盖时回落家族自己的 `top_n()`。
    top_ns: Mutex<HashMap<String, usize>>,
}

impl UnifiedScorer {
    pub fn new(
        families: Vec<Box<dyn CandidateFamily>>,
        priorities: FamilyPriorities,
    ) -> Self {
        UnifiedScorer {
            families,
            priorities,
            top_ns: Mutex::new(HashMap::new()),
        }
    }

    /// 配置某家族进入统一排序的候选宽度(0 = 取消覆盖,回落家族默认)。
    pub fn set_family_top_n(&self, name: &str, n: usize) {
        let mut map = self.top_ns.lock().unwrap();
        if n == 0 {
            map.remove(name);
        } else {
            map.insert(name.to_string(), n);
        }
    }

    /// 家族生效的 top_n(override 优先)。
    pub fn family_top_n(&self, family: &dyn CandidateFamily) -> usize {
        self.top_ns
            .lock()
            .unwrap()
            .get(family.name())
            .copied()
            .unwrap_or_else(|| CandidateFamily::top_n(family))
    }

    /// Rank all candidates (context-free). Returns deduplicated texts.
    pub fn rank(&self, input: &str) -> Vec<String> {
        self.rank_with_context(input, &InputContext::new())
    }

    /// Rank candidates with context. Returns deduplicated texts.
    pub fn rank_with_context(&self, input: &str, ctx: &InputContext) -> Vec<String> {
        self.rank_detailed(input, ctx)
            .into_iter()
            .map(|d| d.text)
            .collect()
    }

    /// Rank with full detail — each result includes source and score.
    /// 便捷组合(`collect` + `merge`);三阶段管线里两段分属 stage2/stage3。
    pub fn rank_detailed(&self, input: &str, ctx: &InputContext) -> Vec<RankedCandidate> {
        self.merge(self.collect(input, ctx))
    }

    /// Stage 2:家族收集 —— 每家 `predict`(raw 分已按家族内语义排序)+
    /// `top_n` 引擎预过滤。**不做跨家族合成**(×priority/全局排序/去重属
    /// stage3 后处理,见 [`merge`](Self::merge))。
    pub fn collect(&self, input: &str, ctx: &InputContext) -> Vec<FamilyCandidates> {
        if input.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for family in &self.families {
            if !family.enabled() {
                continue;
            }
            let priority = self
                .priorities
                .get(family.name())
                .unwrap_or_else(|| family.priority());
            let mut candidates = family.predict(input, ctx);
            candidates.sort_by(|a, b| {
                b.raw_score
                    .partial_cmp(&a.raw_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            candidates.truncate(self.family_top_n(&**family));
            out.push(FamilyCandidates {
                bonus: priority as f64 / 100.0,
                candidates,
            });
        }
        out
    }

    /// Stage 3 合成(后处理第一步):×priority 乘数、全局排序、跨家族去重。
    pub fn merge(&self, collected: Vec<FamilyCandidates>) -> Vec<RankedCandidate> {
        let mut scored: Vec<(f64, RankedCandidate)> = Vec::new();
        for fc in collected {
            for c in fc.candidates {
                let final_score = c.raw_score * fc.bonus;
                scored.push((
                    final_score,
                    RankedCandidate {
                        text: c.text,
                        score: final_score,
                        family: c.family,
                        source: c.source,
                    },
                ));
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut seen = HashSet::new();
        scored
            .into_iter()
            .filter(|(_, rc)| seen.insert(rc.text.clone()))
            .map(|(_, rc)| rc)
            .collect()
    }

    /// Number of registered families (including disabled ones).
    pub fn family_count(&self) -> usize {
        self.families.len()
    }

    /// Access a family by name.
    pub fn family(&self, name: &str) -> Option<&dyn CandidateFamily> {
        self.families
            .iter()
            .find(|f| f.name() == name)
            .map(|f| &**f)
    }

    /// Load an external dictionary into the named family.
    pub fn load_dict_to(&self, family_name: &str, path: &str) -> Option<std::io::Result<usize>> {
        self.family(family_name).map(|f| f.load_dict(path))
    }

    /// Load a user dictionary into the named family (all entries override
    /// earlier loads for the same keyword).
    pub fn load_user_dict_to(
        &self,
        family_name: &str,
        path: &str,
    ) -> Option<std::io::Result<usize>> {
        self.family(family_name).map(|f| f.load_user_dict(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubFamily {
        name: &'static str,
        priority: u32,
        candidates: Vec<(&'static str, f64)>,
    }

    impl CandidateFamily for StubFamily {
        fn name(&self) -> &'static str {
            self.name
        }
        fn priority(&self) -> u32 {
            self.priority
        }
        fn predict(&self, _input: &str, _ctx: &InputContext) -> Vec<ScoredCandidate> {
            self.candidates
                .iter()
                .map(|(t, s)| ScoredCandidate {
                    text: t.to_string(),
                    family: self.name,
                    source: "stub",
                    raw_score: *s,
                })
                .collect()
        }
    }

    #[test]
    fn higher_priority_wins_tie() {
        let fam_a = StubFamily {
            name: "A",
            priority: 100,
            candidates: vec![("word", 0.5)],
        };
        let fam_b = StubFamily {
            name: "B",
            priority: 50,
            candidates: vec![("word", 0.5)],
        };
        let scorer = UnifiedScorer::new(
            vec![Box::new(fam_a), Box::new(fam_b)],
            FamilyPriorities::default(),
        );
        let result = scorer.rank("test");
        assert_eq!(result.len(), 1); // deduped to one
        assert_eq!(result[0], "word");
    }

    #[test]
    fn higher_score_within_family_wins() {
        let fam = StubFamily {
            name: "P",
            priority: 100,
            candidates: vec![("low", 0.3), ("high", 0.9), ("mid", 0.6)],
        };
        let scorer = UnifiedScorer::new(
            vec![Box::new(fam)],
            FamilyPriorities::default(),
        );
        let result = scorer.rank("test");
        assert_eq!(&result[..3], &["high", "mid", "low"]);
    }

    #[test]
    fn cross_family_ranking() {
        let pinyin = StubFamily {
            name: "pinyin",
            priority: 100,
            candidates: vec![("候选A", 0.7)],
        };
        let english = StubFamily {
            name: "english",
            priority: 60,
            candidates: vec![
                ("black", 1.0), // raw 1.0 × 0.60 = 0.60 final
            ],
        };
        let scorer = UnifiedScorer::new(
            vec![Box::new(pinyin), Box::new(english)],
            FamilyPriorities::default(),
        );
        let result = scorer.rank("bla");
        // 候选A: 0.7 × 1.0 = 0.70, black: 1.0 × 0.6 = 0.60
        assert_eq!(result[0], "候选A");
        assert_eq!(result[1], "black");
    }

    #[test]
    fn disabled_family_skipped() {        struct DisabledFamily;
        impl CandidateFamily for DisabledFamily {
            fn name(&self) -> &'static str {
                "disabled"
            }
            fn priority(&self) -> u32 {
                100
            }
            fn enabled(&self) -> bool {
                false
            }
            fn predict(&self, _: &str, _: &InputContext) -> Vec<ScoredCandidate> {
                vec![ScoredCandidate {
                    text: "nope".into(),
                    family: "disabled",
                    source: "stub",
                    raw_score: 1.0,
                }]
            }
        }
        let fam = StubFamily {
            name: "ok",
            priority: 50,
            candidates: vec![("yes", 0.5)],
        };
        let scorer = UnifiedScorer::new(
            vec![Box::new(DisabledFamily), Box::new(fam)],
            FamilyPriorities::default(),
        );
        let result = scorer.rank("x");
        assert_eq!(result, vec!["yes"]);
    }

    #[test]
    fn empty_input_returns_empty() {
        let fam = StubFamily {
            name: "P",
            priority: 100,
            candidates: vec![("x", 0.5)],
        };
        let scorer = UnifiedScorer::new(
            vec![Box::new(fam)],
            FamilyPriorities::default(),
        );
        assert!(scorer.rank("").is_empty());
    }
}

// ── Submodule declarations ──────────────────────────────────────────────

pub mod scoring;
pub use scoring::{FamilyPriorities, FreqScale, ScoringConfig, PREFIX_DECAY_FREE, prefix_decay};
pub mod emoji;
pub mod english;
pub mod magic;
pub mod pinyin;
