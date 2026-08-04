//! Configuration loader for swift-ime.yaml.
//!
//! Resolves via shared::FileLoader (CONF namespace):
//!   dev:  <crate>/swift-ime.yaml
//!   prod: ~/.desk-pilot/swift-ime.yaml

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
}

/// English family priority in the global ranking (0-100). The other families'
/// priorities (pinyin/magic/snippet) are fixed in the engine's scorer.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FamilyPriorityConfig {
    #[serde(default = "default_70")] pub english: u32,
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

fn default_70() -> u32 { 70 }
fn default_0_88() -> f64 { 0.88 }
fn default_0_85() -> f64 { 0.85 }
fn default_0_55() -> f64 { 0.55 }
fn default_0_5() -> f64 { 0.5 }
fn default_0_25() -> f64 { 0.25 }
fn default_0_6() -> f64 { 0.6 }
fn default_0_12() -> f64 { 0.12 }
fn default_0_05() -> f64 { 0.05 }
fn default_0_01() -> f64 { 0.01 }
fn default_48() -> usize { 48 }
fn default_96() -> usize { 96 }
fn default_8() -> usize { 8 }
fn default_true() -> bool { true }
fn default_page_size() -> u32 { 7 }

impl Default for WeightsConfig {
    fn default() -> Self {
        WeightsConfig {
            family_priority: FamilyPriorityConfig { english: 70 },
            pinyin: PinyinWeightConfig {
                phrase_book: 0.88, large_dict: 0.85, viterbi_base: 0.25, viterbi_scale: 0.55,
                jianpin: 0.50, single_syl_decay: 0.5, context_boost: 0.12,
                stopword_penalty: 0.5, confirm_bonus: 0.05, short_word_bonus: 0.01,
                large_dict_take: 96, viterbi_take: 48, jianpin_take: 8,
            },
            english: EnglishWeightConfig { exact: 0.88, prefix_ratio: 0.6, user_boost: 1.0 },
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
        }
    }
}

impl SwiftImeConfig {
    /// Load from the CONF namespace via FileLoader, falling back to defaults.
    pub fn load() -> Self {
        let loader = shared::loader!(".");
        match loader.resolve("CONF::swift-ime.yaml") {
            Some(path) => {
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
