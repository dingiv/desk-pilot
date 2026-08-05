//! Configuration loader for swift-ime.yaml.
//!
//! Resolves via shared::FileLoader (CONF namespace):
//!   dev:  <crate>/swift-ime.yaml
//!   prod: ~/.desk-pilot/swift-ime.yaml

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SwiftImeConfig {
    #[serde(default)]
    pub dicts: DictsConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub weights: WeightsConfig,
    #[serde(default)]
    pub magic: MagicConfig,
    /// User-defined snippets (merged over the engine's built-ins on trigger
    /// collisions — a config `/sig` replaces the built-in one). Expansions
    /// support `$DATE` / `$CLIPBOARD` / `$CURSOR` variables.
    #[serde(default)]
    pub snippets: Vec<SnippetEntryConfig>,
}

/// One user-defined snippet trigger → expansion template.
#[derive(Debug, Clone, Deserialize)]
pub struct SnippetEntryConfig {
    /// The trigger string (e.g. "/sig", "#hello").
    #[serde(default)]
    pub trigger: String,
    /// The expansion text — may contain `$DATE`, `$CLIPBOARD`, `$CURSOR`.
    #[serde(default)]
    pub expand: String,
}

/// `#req` magic-command backend configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct MagicConfig {
    /// Base URL the `#req` command appends its suffix to
    /// (e.g. `#req/news?query=soccer` → `GET {req_base}/news?query=soccer`).
    #[serde(default = "default_req_base")]
    pub req_base: String,
}

fn default_req_base() -> String {
    ime_core::family::magic::DEFAULT_REQ_BASE.to_string()
}

impl Default for MagicConfig {
    fn default() -> Self {
        MagicConfig { req_base: default_req_base() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeightsConfig {
    #[serde(default)]
    pub family_priority: FamilyPriorityConfig,
    #[serde(default)]
    pub pinyin: PinyinWeightConfig,
    #[serde(default)]
    pub english: EnglishWeightConfig,
    /// RecencyStore 位置衰减 boost(最近提交环的 5 档)。
    #[serde(default)]
    pub recency: RecencyConfig,
    /// UserBigram 上下文 boost 上限。
    #[serde(default)]
    pub bigram: BigramConfig,
    /// 字典词频 → 内部分值的映射参数。
    #[serde(default)]
    pub freq_scale: FreqScaleConfig,
}

/// 各家族的全局优先级(最终分 = raw_score × priority/100)。
/// 只有拼音与英文参与统一打分(`#`/`/` 前缀分流后,魔法与 snippet 不走 scorer)。
#[derive(Debug, Clone, Deserialize)]
pub struct FamilyPriorityConfig {
    #[serde(default = "default_100")] pub pinyin: u32,
    #[serde(default = "default_70")] pub english: u32,
}

impl Default for FamilyPriorityConfig {
    fn default() -> Self {
        FamilyPriorityConfig { pinyin: 100, english: 70 }
    }
}

/// RecencyStore 的位置衰减 boost。
#[derive(Debug, Clone, Deserialize)]
pub struct RecencyConfig {
    #[serde(default = "default_0_20")] pub pos0: f64,
    #[serde(default = "default_0_15")] pub pos1: f64,
    #[serde(default = "default_0_10")] pub pos2: f64,
    #[serde(default = "default_0_05")] pub mid: f64,
    #[serde(default = "default_0_02")] pub far: f64,
}

impl Default for RecencyConfig {
    fn default() -> Self {
        RecencyConfig { pos0: 0.20, pos1: 0.15, pos2: 0.10, mid: 0.05, far: 0.02 }
    }
}

/// UserBigram 上下文 boost 的归一化上限(1.0 = 无加成,1.25 = +25%)。
#[derive(Debug, Clone, Deserialize)]
pub struct BigramConfig {
    #[serde(default = "default_0_25")] pub max_boost: f64,
}

impl Default for BigramConfig {
    fn default() -> Self {
        BigramConfig { max_boost: 0.25 }
    }
}

/// 字典词频 → 内部分值的映射。`max_weight`:
/// - `0`(默认)= auto:用索引构建时记录的实际最大词频(cache v2)——
///   映射对齐真实分布,不同词频的词不会被压成同分;
/// - `> 0` = 显式固定分母。
#[derive(Debug, Clone, Deserialize)]
pub struct FreqScaleConfig {
    #[serde(default)] pub max_weight: f64,
    #[serde(default = "default_0_25")] pub min_score: f64,
    #[serde(default = "default_1_0")] pub max_score: f64,
}

impl Default for FreqScaleConfig {
    fn default() -> Self {
        FreqScaleConfig { max_weight: 0.0, min_score: 0.25, max_score: 1.0 }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PinyinWeightConfig {
    #[serde(default = "default_0_88")] pub phrase_book: f64,
    #[serde(default = "default_0_85")] pub large_dict: f64,
    #[serde(default = "default_0_25")] pub viterbi_base: f64,
    #[serde(default = "default_0_55")] pub viterbi_scale: f64,
    #[serde(default = "default_0_5")] pub jianpin: f64,
    #[serde(default = "default_0_5")] pub single_syl_decay: f64,
    #[serde(default = "default_0_12")] pub context_boost: f64,
    #[serde(default = "default_0_5")] pub stopword_penalty: f64,
    #[serde(default = "default_0_05")] pub confirm_bonus: f64,
    #[serde(default = "default_0_01")] pub short_word_bonus: f64,
    #[serde(default = "default_96")] pub large_dict_take: usize,
    #[serde(default = "default_48")] pub viterbi_take: usize,
    #[serde(default = "default_8")] pub jianpin_take: usize,
}

impl WeightsConfig {
    /// Convert to ime-core's unified [`ScoringConfig`] — family priorities,
    /// recency boosts, bigram ceiling and the freq→score scale all come from
    /// `swift-ime.yaml`; missing sections fall back to the legacy defaults.
    pub fn to_scoring(&self) -> ime_core::scoring::ScoringConfig {
        ime_core::scoring::ScoringConfig {
            priorities: ime_core::scoring::FamilyPriorities {
                pinyin: self.family_priority.pinyin,
                english: self.family_priority.english,
            },
            recency: ime_core::scoring::RecencyBoosts {
                pos0: self.recency.pos0,
                pos1: self.recency.pos1,
                pos2: self.recency.pos2,
                mid: self.recency.mid,
                far: self.recency.far,
            },
            bigram: ime_core::scoring::BigramTuning { max_boost: self.bigram.max_boost },
            freq_scale: ime_core::scoring::FreqScale {
                max_weight: self.freq_scale.max_weight,
                min_score: self.freq_scale.min_score,
                max_score: self.freq_scale.max_score,
            },
        }
    }
}

impl PinyinWeightConfig {
    /// Convert to ime-core's PinyinWeights for engine construction.
    pub fn to_engine(&self) -> ime_core::family::pinyin::PinyinWeights {
        ime_core::family::pinyin::PinyinWeights {
            phrase_book: self.phrase_book,
            large_dict: self.large_dict,
            viterbi_base: self.viterbi_base,
            viterbi_scale: self.viterbi_scale,
            jianpin: self.jianpin,
            single_syl_decay: self.single_syl_decay,
            context_boost: self.context_boost,
            stopword_penalty: self.stopword_penalty,
            confirm_bonus: self.confirm_bonus,
            short_word_bonus: self.short_word_bonus,
            large_dict_take: self.large_dict_take,
            viterbi_take: self.viterbi_take,
            jianpin_take: self.jianpin_take,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnglishWeightConfig {
    #[serde(default = "default_0_88")] pub exact: f64,
    #[serde(default = "default_0_6")] pub prefix_ratio: f64,
    #[serde(default = "default_1_0")] pub user_boost: f64,
}

fn default_1_0() -> f64 { 1.0 }

fn default_100() -> u32 { 100 }
fn default_70() -> u32 { 70 }
fn default_0_88() -> f64 { 0.88 }
fn default_0_85() -> f64 { 0.85 }
fn default_0_55() -> f64 { 0.55 }
fn default_0_5() -> f64 { 0.5 }
fn default_0_25() -> f64 { 0.25 }
fn default_0_20() -> f64 { 0.20 }
fn default_0_15() -> f64 { 0.15 }
fn default_0_10() -> f64 { 0.10 }
fn default_0_6() -> f64 { 0.6 }
fn default_0_12() -> f64 { 0.12 }
fn default_0_05() -> f64 { 0.05 }
fn default_0_02() -> f64 { 0.02 }
fn default_0_01() -> f64 { 0.01 }
fn default_48() -> usize { 48 }
fn default_96() -> usize { 96 }
fn default_8() -> usize { 8 }
fn default_true() -> bool { true }
fn default_page_size() -> u32 { 7 }

impl Default for WeightsConfig {
    fn default() -> Self {
        WeightsConfig {
            family_priority: FamilyPriorityConfig::default(),
            pinyin: PinyinWeightConfig {
                phrase_book: 0.88, large_dict: 0.85, viterbi_base: 0.25, viterbi_scale: 0.55,
                jianpin: 0.50, single_syl_decay: 0.5, context_boost: 0.12,
                stopword_penalty: 0.5, confirm_bonus: 0.05, short_word_bonus: 0.01,
                large_dict_take: 96, viterbi_take: 48, jianpin_take: 8,
            },
            english: EnglishWeightConfig { exact: 0.88, prefix_ratio: 0.6, user_boost: 1.0 },
            recency: RecencyConfig {
                pos0: 0.20, pos1: 0.15, pos2: 0.10, mid: 0.05, far: 0.02,
            },
            bigram: BigramConfig { max_boost: 0.25 },
            freq_scale: FreqScaleConfig { max_weight: 0.0, min_score: 0.25, max_score: 1.0 },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DictsConfig {
    #[serde(default = "default_true")]
    pub base: bool,
    #[serde(default = "default_true")]
    pub rime_ice: bool,
}

impl Default for DictsConfig {
    fn default() -> Self { DictsConfig { base: true, rime_ice: true } }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct InputConfig {
    #[serde(default = "default_true")]
    pub fuzzy: bool,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

impl Default for SwiftImeConfig {
    fn default() -> Self {
        SwiftImeConfig {
            dicts: DictsConfig::default(),
            input: InputConfig { fuzzy: true, page_size: 7 },
            weights: WeightsConfig::default(),
            magic: MagicConfig::default(),
            snippets: Vec::new(),
        }
    }
}

/// System config template, installed by the .deb to `/usr/share/swift-ime/`. On first run (no
/// user config yet) it is copied to the user's CONF dir, then loaded — the user can edit their
/// copy; a template update never overwrites it.
const TEMPLATE_PATH: &str = "/usr/share/swift-ime/swift-ime.yaml";

impl SwiftImeConfig {
    /// Load from the CONF namespace via FileLoader, falling back to defaults.
    ///
    /// First run: if the user config doesn't exist yet, copy the system template
    /// (`/usr/share/swift-ime/swift-ime.yaml`, installed by the .deb) into place — so a fresh
    /// install boots with real config, not defaults. The user's copy is never overwritten.
    ///
    /// Note: `FileLoader::resolve_ns` returns the CONF path even when the file doesn't exist
    /// (it serves write scenarios like the .log / user-dict files). So the existence check is
    /// here, not in the loader — "no config" must not be reported as a read error.
    pub fn load() -> Self {
        let loader = shared::loader!(".");
        match loader.resolve("CONF::swift-ime.yaml") {
            Some(path) => {
                // First run: seed the user config from the system template.
                if !path.exists() {
                    match seed_from_template(&path, Path::new(TEMPLATE_PATH)) {
                        SeedOutcome::Copied => {
                            eprintln!("[swift-ime] seeded user config from template → {}", path.display());
                        }
                        SeedOutcome::TemplateMissing => {
                            eprintln!("[swift-ime] no user config, template missing ({TEMPLATE_PATH}), using defaults");
                            return SwiftImeConfig::default();
                        }
                        SeedOutcome::CopyFailed(e) => {
                            eprintln!("[swift-ime] template copy failed: {e}, using defaults");
                            return SwiftImeConfig::default();
                        }
                    }
                }
                match std::fs::read_to_string(&path) {
                    Ok(yaml) => {
                        match serde_yaml::from_str(&yaml) {
                            Ok(cfg) => {
                                eprintln!("[swift-ime] loaded config from {}", path.display());
                                return cfg;
                            }
                            Err(e) => eprintln!("[swift-ime] config parse error: {e}, using defaults"),
                        }
                    }
                    Err(e) => eprintln!("[swift-ime] config read error: {e}, using defaults"),
                }
            }
            None => eprintln!("[swift-ime] no config found, using defaults"),
        }
        SwiftImeConfig::default()
    }
}

/// Outcome of seeding the user config from the system template.
#[derive(Debug)]
enum SeedOutcome {
    Copied,
    TemplateMissing,
    CopyFailed(std::io::Error),
}

impl PartialEq for SeedOutcome {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SeedOutcome::Copied, SeedOutcome::Copied) => true,
            (SeedOutcome::TemplateMissing, SeedOutcome::TemplateMissing) => true,
            (SeedOutcome::CopyFailed(a), SeedOutcome::CopyFailed(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

/// Copy the template to `user_path` if the template exists (its parent dir is created first).
/// Never overwrites an existing user config — callers only invoke this when `user_path` is
/// absent. Pure + unit-testable.
fn seed_from_template(user_path: &Path, template: &Path) -> SeedOutcome {
    if !template.exists() {
        return SeedOutcome::TemplateMissing;
    }
    if let Some(parent) = user_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return SeedOutcome::CopyFailed(e);
        }
    }
    match std::fs::copy(template, user_path) {
        Ok(_) => SeedOutcome::Copied,
        Err(e) => SeedOutcome::CopyFailed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_snippets_section() {
        let yaml = r#"
snippets:
  - trigger: "/sig"
    expand: "Best regards, $DATE"
  - trigger: "/clip"
    expand: "Clip: $CLIPBOARD"
"#;
        let cfg: SwiftImeConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.snippets.len(), 2);
        assert_eq!(cfg.snippets[0].trigger, "/sig");
        assert_eq!(cfg.snippets[0].expand, "Best regards, $DATE");
        assert_eq!(cfg.snippets[1].trigger, "/clip");
        assert_eq!(cfg.snippets[1].expand, "Clip: $CLIPBOARD");
    }

    #[test]
    fn snippets_default_to_empty() {
        let cfg = SwiftImeConfig::default();
        assert!(cfg.snippets.is_empty());
    }

    #[test]
    fn parses_scoring_sections() {
        let yaml = r#"
weights:
  family_priority:
    pinyin: 90
    magic: 80
    snippet: 60
    english: 50
  recency:
    pos0: 0.30
    pos1: 0.20
  bigram:
    max_boost: 0.40
  freq_scale:
    max_weight: 500000
    min_score: 0.2
    max_score: 0.95
"#;
        let cfg: SwiftImeConfig = serde_yaml::from_str(yaml).unwrap();
        let s = cfg.weights.to_scoring();
        assert_eq!(s.priorities.pinyin, 90);
        assert_eq!(s.priorities.english, 50);
        assert_eq!(s.recency.pos0, 0.30);
        assert_eq!(s.recency.pos1, 0.20);
        assert_eq!(s.recency.far, 0.02, "未指定的档回退默认");
        assert_eq!(s.bigram.max_boost, 0.40);
        assert_eq!(s.freq_scale.max_weight, 500_000.0);
        assert_eq!(s.freq_scale.min_score, 0.2);
        assert_eq!(s.freq_scale.max_score, 0.95);
    }

    #[test]
    fn scoring_sections_default_when_missing() {
        // 旧配置文件没有这些节 → 全部用引擎默认(与写死值一致)。
        let yaml = "weights:\n  pinyin: {}\n";
        let cfg: SwiftImeConfig = serde_yaml::from_str(yaml).unwrap();
        let s = cfg.weights.to_scoring();
        assert_eq!(s.priorities.pinyin, 100);
        assert_eq!(s.priorities.english, 70);
        assert_eq!(s.recency.pos0, 0.20);
        assert_eq!(s.bigram.max_boost, 0.25);
        assert_eq!(s.freq_scale.max_weight, 0.0, "auto by default");
    }

    #[test]
    fn seed_copies_template_when_user_config_absent() {
        let dir = std::env::temp_dir().join(format!("swift-ime-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tpl = dir.join("tpl.yaml");
        std::fs::write(&tpl, "input: { fuzzy: false }\n").unwrap();
        let user = dir.join("nested/dir/swift-ime.yaml"); // parent doesn't exist yet

        assert_eq!(seed_from_template(&user, &tpl), SeedOutcome::Copied);
        assert!(user.exists(), "user config seeded");
        assert_eq!(std::fs::read_to_string(&user).unwrap(), "input: { fuzzy: false }\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_reports_missing_template() {
        let dir = std::env::temp_dir().join(format!("swift-ime-noseed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("no-tpl.yaml");
        let user = dir.join("swift-ime.yaml");
        assert_eq!(seed_from_template(&user, &missing), SeedOutcome::TemplateMissing);
        assert!(!user.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
