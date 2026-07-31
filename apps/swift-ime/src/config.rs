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
    pub families: FamiliesConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub weights: WeightsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeightsConfig {
    #[serde(default)]
    pub family_priority: FamilyPriorityConfig,
    #[serde(default)]
    pub pinyin: PinyinWeightConfig,
    #[serde(default)]
    pub snippet: SnippetWeightConfig,
    #[serde(default)]
    pub magic: MagicWeightConfig,
    #[serde(default)]
    pub english: EnglishWeightConfig,
    #[serde(default)]
    pub display: DisplayConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FamilyPriorityConfig {
    #[serde(default = "default_100")] pub pinyin: u32,
    #[serde(default = "default_95")] pub magic: u32,
    #[serde(default = "default_75")] pub snippet: u32,
    #[serde(default = "default_60")] pub english: u32,
    #[serde(default = "default_50")] pub emoji: u32,
    #[serde(default = "default_40")] pub ai: u32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PinyinWeightConfig {
    #[serde(default = "default_1_0")] pub phrase_book: f64,
    #[serde(default = "default_0_95")] pub large_dict: f64,
    #[serde(default = "default_0_3")] pub viterbi_base: f64,
    #[serde(default = "default_0_65")] pub viterbi_scale: f64,
    #[serde(default = "default_0_5")] pub session: f64,
    #[serde(default = "default_0_3")] pub prefix: f64,
    #[serde(default = "default_0_85")] pub phrase_book_prefix: f64,
    #[serde(default = "default_0_7")] pub jianpin: f64,
    #[serde(default = "default_0_6")] pub single_syl_decay: f64,
    #[serde(default = "default_0_15")] pub context_boost: f64,
    #[serde(default = "default_0_5")] pub stopword_penalty: f64,
    #[serde(default = "default_0_05")] pub confirm_bonus: f64,
    #[serde(default = "default_0_02")] pub short_word_bonus: f64,
    #[serde(default = "default_96")] pub large_dict_take: usize,
    #[serde(default = "default_48")] pub viterbi_take: usize,
    #[serde(default = "default_8")] pub jianpin_take: usize,
    #[serde(default = "default_256")] pub prefix_take: usize,
}

impl PinyinWeightConfig {
    /// Convert to ime-core's PinyinWeights for engine construction.
    pub fn to_engine(&self) -> ime_core::family::pinyin::PinyinWeights {
        ime_core::family::pinyin::PinyinWeights {
            phrase_book: self.phrase_book,
            large_dict: self.large_dict,
            viterbi_base: self.viterbi_base,
            viterbi_scale: self.viterbi_scale,
            session: self.session,
            prefix: self.prefix,
            phrase_book_prefix: self.phrase_book_prefix,
            jianpin: self.jianpin,
            single_syl_decay: self.single_syl_decay,
            context_boost: self.context_boost,
            stopword_penalty: self.stopword_penalty,
            confirm_bonus: self.confirm_bonus,
            short_word_bonus: self.short_word_bonus,
            large_dict_take: self.large_dict_take,
            viterbi_take: self.viterbi_take,
            jianpin_take: self.jianpin_take,
            prefix_take: self.prefix_take,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SnippetWeightConfig {
    #[serde(default = "default_1_0")] pub exact: f64,
    #[serde(default = "default_0_5")] pub partial: f64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MagicWeightConfig {
    #[serde(default = "default_1_0")] pub exact: f64,
    #[serde(default = "default_0_9")] pub prefix: f64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnglishWeightConfig {
    #[serde(default = "default_1_0")] pub exact: f64,
    #[serde(default = "default_0_7")] pub prefix_ratio: f64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DisplayConfig {
    #[serde(default = "default_8")] pub max_full_comps: usize,
    #[serde(default = "default_8")] pub max_char_cands: usize,
}

fn default_100() -> u32 { 100 }
fn default_95() -> u32 { 95 }
fn default_75() -> u32 { 75 }
fn default_60() -> u32 { 60 }
fn default_50() -> u32 { 50 }
fn default_40() -> u32 { 40 }
fn default_1_0() -> f64 { 1.0 }
fn default_0_95() -> f64 { 0.95 }
fn default_0_9() -> f64 { 0.9 }
fn default_0_85() -> f64 { 0.85 }
fn default_0_7() -> f64 { 0.7 }
fn default_0_65() -> f64 { 0.65 }
fn default_0_6() -> f64 { 0.6 }
fn default_0_5() -> f64 { 0.5 }
fn default_0_3() -> f64 { 0.3 }
fn default_0_15() -> f64 { 0.15 }
fn default_0_05() -> f64 { 0.05 }
fn default_0_02() -> f64 { 0.02 }
fn default_8() -> usize { 8 }
fn default_48() -> usize { 48 }
fn default_96() -> usize { 96 }
fn default_256() -> usize { 256 }
fn default_true() -> bool { true }
fn default_page_size() -> u32 { 7 }

impl Default for WeightsConfig {
    fn default() -> Self {
        WeightsConfig {
            family_priority: FamilyPriorityConfig {
                pinyin: 100, magic: 95, snippet: 75, english: 60, emoji: 50, ai: 40,
            },
            pinyin: PinyinWeightConfig {
                phrase_book: 1.0, large_dict: 0.95, viterbi_base: 0.3, viterbi_scale: 0.65,
                session: 0.5, prefix: 0.3, phrase_book_prefix: 0.85, jianpin: 0.70,
                single_syl_decay: 0.6, context_boost: 0.15,
                stopword_penalty: 0.5, confirm_bonus: 0.05, short_word_bonus: 0.02,
                large_dict_take: 96, viterbi_take: 48, jianpin_take: 8, prefix_take: 256,
            },
            snippet: SnippetWeightConfig { exact: 1.0, partial: 0.5 },
            magic: MagicWeightConfig { exact: 1.0, prefix: 0.9 },
            english: EnglishWeightConfig { exact: 1.0, prefix_ratio: 0.7 },
            display: DisplayConfig { max_full_comps: 8, max_char_cands: 8 },
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
pub struct FamiliesConfig {
    #[serde(default = "default_true")] pub pinyin: bool,
    #[serde(default = "default_true")] pub english: bool,
    #[serde(default = "default_true")] pub magic: bool,
    #[serde(default = "default_true")] pub snippet: bool,
    #[serde(default)] pub emoji: bool,
    #[serde(default)] pub ai: bool,
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
            families: FamiliesConfig::default(),
            input: InputConfig { fuzzy: true, page_size: 7 },
            weights: WeightsConfig::default(),
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
