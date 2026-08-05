//! SnippetFamily — user-defined text expansion via trie matching.
//!
//! Activated when input starts with `/` (NOT `#` — that's the MagicFamily).
//! The existing [`Matcher`] trie + [`Expander`] variable substitution are
//! wrapped as a [`CandidateFamily`].

use crate::expander::Expander;
use super::{CandidateFamily, ScoredCandidate};
use crate::matcher::{Match, Matcher};

pub struct SnippetFamily {
    matcher: Matcher,
    expander: Expander,
    enabled: bool,
}

impl SnippetFamily {
    pub fn new(matcher: Matcher, expander: Expander) -> Self {
        SnippetFamily { matcher, expander, enabled: true }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Expose the matcher for FSM trigger detection.
    pub fn matcher(&self) -> &Matcher {
        &self.matcher
    }

    /// Check if `ch` is a snippet trigger prefix (`/` only; `#` is Magic).
    pub fn is_trigger(ch: char) -> bool {
        ch == '/'
    }

    /// Walk the trie step by step (used by FSM during composition).
    pub fn step(&self, prefix: &str, ch: char) -> Match {
        self.matcher.step(prefix, ch)
    }
}

impl CandidateFamily for SnippetFamily {
    fn name(&self) -> &'static str {
        "snippet"
    }

    fn priority(&self) -> u32 {
        75
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn top_n(&self) -> usize {
        4
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() || !input.starts_with('/') {
            return Vec::new();
        }

        // Walk the trie char by char.
        let mut prefix = String::new();
        for (i, ch) in input.chars().enumerate() {
            if i == 0 {
                prefix.push(ch); // first char is '/'
                continue;
            }
            match self.matcher.step(&prefix, ch) {
                Match::Complete { expansion, .. } => {
                    // Found exact match — expand and return.
                    let expanded = self.expander.expand(&expansion)
                        .unwrap_or_else(|_| expansion);
                    return vec![ScoredCandidate {
                        text: expanded,
                        family: "snippet", source: "exact",
                        raw_score: 1.0,
                    }];
                }
                Match::Partial => {
                    prefix.push(ch);
                }
                Match::None => {
                    // Dead end, but the accumulated prefix (including ch)
                    // will be committed raw by the FSM.
                    return Vec::new();
                }
            }
        }

        // If we get here, the input is a partial trigger (e.g., "/gre").
        // Return the partial text as a preedit hint.
        if prefix.len() > 1 {
            return vec![ScoredCandidate {
                text: prefix.clone(),
                family: "snippet", source: "partial",
                raw_score: 0.5,
            }];
        }

        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expander::StaticProvider;

    fn make_family() -> SnippetFamily {
        let entries = vec![
            ("/greet".into(), "你好，我是 AI 秘书".into()),
            ("/sig".into(), "Best regards,\nAlice".into()),
        ];
        let expander = Expander::new(std::sync::Arc::new(StaticProvider {
            date: "2026-07-27".into(),
            clipboard: String::new(),
        }));
        SnippetFamily::new(Matcher::new(entries), expander)
    }

    #[test]
    fn exact_match() {
        let fam = make_family();
        let cands = fam.predict("/greet");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].text, "你好，我是 AI 秘书");
        assert!((cands[0].raw_score - 1.0).abs() < 0.001);
    }

    #[test]
    fn partial_match() {
        let fam = make_family();
        let cands = fam.predict("/gre");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].text, "/gre");
        assert!((cands[0].raw_score - 0.5).abs() < 0.001);
    }

    #[test]
    fn no_match() {
        let fam = make_family();
        let cands = fam.predict("/xyz");
        assert!(cands.is_empty());
    }

    #[test]
    fn only_activated_by_slash() {
        let fam = make_family();
        assert!(fam.predict("hello").is_empty());
        assert!(fam.predict("#date").is_empty());
    }
}
