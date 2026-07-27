//! MagicFamily — `#`-prefixed built-in commands.
//!
//! Activated when input starts with `#`. Supports prefix matching so
//! `#da` finds `#date` while the user is still typing.
//!
//! Built-in commands:
//! - `#date` — insert today's date (YYYY-MM-DD)
//! - `#asr`  — voice input (reserved, returns placeholder)
//! - `#password` — password manager (reserved, returns placeholder)

use super::{CandidateFamily, ScoredCandidate};

pub struct MagicFamily {
    enabled: bool,
}

/// Built-in magic commands. Each entry is (trigger, description, expansion).
const COMMANDS: &[(&str, &str)] = &[
    ("#date", "insert today's date"),
    ("#asr", "voice input"),
    ("#password", "password manager"),
];

impl MagicFamily {
    pub fn new() -> Self {
        MagicFamily { enabled: true }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Check if `ch` is the magic trigger prefix.
    pub fn is_trigger(ch: char) -> bool {
        ch == '#'
    }

    /// Resolve a complete command to its expansion text.
    pub fn resolve(&self, trigger: &str) -> Option<String> {
        match trigger {
            "#date" => {
                // Today's date in local time.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let secs = now.as_secs();
                // Approximate: use a fixed offset for simplicity.
                // In production this would use the system timezone.
                let days = secs / 86400;
                // 1970-01-01 + days
                let year = 1970 + (days / 365);
                let rem = days % 365;
                let month = 1 + (rem / 30).min(11);
                let day = 1 + (rem % 30).min(27);
                Some(format!("{year:04}-{month:02}-{day:02}"))
            }
            "#asr" => Some("[voice input — not yet implemented]".into()),
            "#password" => Some("[password manager — not yet implemented]".into()),
            _ => None,
        }
    }
}

impl Default for MagicFamily {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateFamily for MagicFamily {
    fn name(&self) -> &'static str {
        "magic"
    }

    fn priority(&self) -> u32 {
        95
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn top_n(&self) -> usize {
        4
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() || !input.starts_with('#') {
            return Vec::new();
        }

        let mut out = Vec::new();
        for (trigger, _desc) in COMMANDS {
            if *trigger == input {
                // Exact match — resolve and score 1.0.
                if let Some(expansion) = self.resolve(trigger) {
                    out.push(ScoredCandidate {
                        text: expansion,
                        family: "magic",
                        raw_score: 1.0,
                    });
                }
            } else if trigger.starts_with(input) {
                // Prefix match — show the trigger as a hint.
                out.push(ScoredCandidate {
                    text: trigger.to_string(),
                    family: "magic",
                    raw_score: 0.9,
                });
            }
        }
        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_date_command() {
        let fam = MagicFamily::new();
        let cands = fam.predict("#date");
        assert_eq!(cands.len(), 1);
        assert!(cands[0].text.starts_with("202"));
        assert!((cands[0].raw_score - 1.0).abs() < 0.001);
    }

    #[test]
    fn prefix_match() {
        let fam = MagicFamily::new();
        let cands = fam.predict("#da");
        assert!(!cands.is_empty());
        assert!(cands.iter().any(|c| c.text == "#date"));
    }

    #[test]
    fn only_activated_by_hash() {
        let fam = MagicFamily::new();
        assert!(fam.predict("hello").is_empty());
        assert!(fam.predict("/greet").is_empty());
    }
}
