//! EnglishFamily — English word prediction via prefix matching.
//!
//! Activated when input contains character sequences that can't be valid
//! pinyin syllables. Uses an embedded dictionary of ~200 common English
//! words for prefix matching.

use super::{CandidateFamily, ScoredCandidate};

/// A small embedded English dictionary. We keep it small (~200 words) to
/// stay fast. A larger dictionary can be loaded from a file later.
const ENGLISH_WORDS: &[&str] = &[
    "about", "above", "after", "again", "all", "also", "always", "and", "any", "are",
    "around", "back", "been", "before", "best", "better", "big", "black", "blue", "both",
    "bring", "brown", "but", "call", "came", "can", "change", "children", "city", "close",
    "cold", "come", "could", "country", "create", "dark", "day", "did", "different", "do",
    "does", "done", "down", "each", "earth", "end", "enough", "even", "ever", "every",
    "example", "family", "far", "father", "feel", "few", "find", "first", "follow", "food",
    "for", "form", "found", "four", "from", "full", "gave", "get", "give", "go",
    "going", "good", "got", "great", "green", "group", "had", "hand", "hard", "has",
    "have", "head", "help", "her", "here", "high", "him", "his", "hold", "home",
    "house", "how", "idea", "important", "into", "just", "keep", "kind", "know", "large",
    "last", "late", "learn", "left", "less", "let", "life", "light", "like", "line",
    "little", "live", "long", "look", "lot", "love", "made", "make", "man", "many",
    "may", "mean", "men", "might", "mile", "miss", "money", "month", "more", "morning",
    "most", "mother", "move", "much", "music", "must", "name", "near", "need", "never",
    "new", "next", "night", "number", "off", "often", "old", "once", "one", "only",
    "open", "other", "our", "out", "over", "own", "page", "paper", "part", "people",
    "picture", "place", "plant", "play", "point", "power", "put", "question", "read", "real",
    "really", "red", "right", "river", "room", "run", "said", "same", "saw", "say",
    "school", "sea", "second", "see", "seem", "set", "she", "should", "show", "side",
    "since", "small", "soon", "sound", "spell", "stand", "start", "state", "still", "stop",
    "story", "study", "such", "system", "take", "talk", "tell", "than", "that", "the",
    "their", "them", "then", "there", "these", "they", "thing", "think", "this", "those",
    "though", "thought", "three", "through", "time", "today", "together", "told", "too", "took",
    "tree", "try", "turn", "two", "under", "until", "upon", "use", "used", "very",
    "walk", "want", "war", "water", "way", "went", "were", "what", "when", "where",
    "which", "while", "white", "who", "why", "will", "with", "without", "word", "work",
    "world", "would", "write", "year", "you", "young",
];

pub struct EnglishFamily {
    enabled: bool,
}

impl EnglishFamily {
    pub fn new() -> Self {
        EnglishFamily { enabled: true }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl Default for EnglishFamily {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateFamily for EnglishFamily {
    fn name(&self) -> &'static str {
        "english"
    }

    fn priority(&self) -> u32 {
        60
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn top_n(&self) -> usize {
        4
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() {
            return Vec::new();
        }

        let input_lower = input.to_ascii_lowercase();
        let mut out = Vec::new();

        for word in ENGLISH_WORDS {
            if *word == input_lower {
                out.push(ScoredCandidate {
                    text: word.to_string(),
                    family: "english", source: "exact",
                    raw_score: 1.0,
                });
            } else if word.starts_with(&input_lower) {
                let score = input_lower.len() as f64 / word.len() as f64;
                out.push(ScoredCandidate {
                    text: word.to_string(),
                    family: "english", source: "prefix",
                    raw_score: score.clamp(0.3, 0.95),
                });
            }
        }

        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out.truncate(16);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_black() {
        let fam = EnglishFamily::new();
        let cands = fam.predict("black");
        assert!(cands.iter().any(|c| c.text == "black" && (c.raw_score - 1.0).abs() < 0.001));
    }

    #[test]
    fn prefix_blac() {
        let fam = EnglishFamily::new();
        let cands = fam.predict("blac");
        assert!(cands.iter().any(|c| c.text == "black"));
        // "black" should be top since blac/black = 4/5 = 0.8
        assert_eq!(cands[0].text, "black");
    }

    #[test]
    fn prefix_hel() {
        let fam = EnglishFamily::new();
        let cands = fam.predict("hel");
        assert!(cands.iter().any(|c| c.text == "help"));
    }

    #[test]
    fn empty_returns_nothing() {
        let fam = EnglishFamily::new();
        assert!(fam.predict("").is_empty());
    }

    #[test]
    fn no_match_garbage() {
        let fam = EnglishFamily::new();
        let cands = fam.predict("zzzzz");
        assert!(cands.is_empty());
    }
}
