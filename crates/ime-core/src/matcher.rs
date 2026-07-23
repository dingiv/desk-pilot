//! Trie-based prefix matcher for snippet triggers. Thread-safe, lock-free reads after build.

use std::collections::HashMap;

/// The result of one character fed to the Matcher.
#[derive(Debug, Clone, PartialEq)]
pub enum Match {
    /// Prefix still matches at least one trigger — keep accumulating.
    Partial,
    /// A complete trigger was matched; return the expansion text.
    Complete { trigger: String, expansion: String },
    /// Dead end — no trigger has this prefix. Reset the state.
    None,
}

/// A trie node for prefix matching. Each node may carry a completion (if a trigger
/// ends at this node) and a map of next characters.
#[derive(Debug, Clone, Default)]
struct Node {
    /// If this node completes a trigger, the expansion text.
    expansion: Option<String>,
    /// The original trigger (needed for the Complete result).
    trigger: Option<String>,
    /// Child nodes keyed by the next character.
    children: HashMap<char, Node>,
}

/// Builds a [`Matcher`] from a list of (trigger, expansion) pairs.
///
/// Triggers are matched **longest-prefix**: if both "/g" and "/greet" are defined,
/// typing "/greet" matches "/greet" (not "/g") as soon as the final character is
/// unambiguous. If "/g" is the only match after typing "/g", it completes immediately
/// (no extra keystroke needed when unambiguous).
#[derive(Debug, Clone)]
pub struct Matcher {
    root: Node,
    /// The special trigger prefix(es) that activate snippet mode. Default: '/', '#'.
    trigger_prefixes: Vec<char>,
}

impl Matcher {
    /// Build a matcher from a list of `(trigger, expansion)` pairs. An empty list
    /// produces a no-op matcher (all queries return `Match::None`).
    pub fn new(entries: Vec<(String, String)>) -> Self {
        let mut root = Node::default();
        for (trigger, expansion) in entries {
            let mut node = &mut root;
            for ch in trigger.chars() {
                node = node.children.entry(ch).or_default();
            }
            node.expansion = Some(expansion.clone());
            node.trigger = Some(trigger);
        }
        Matcher {
            root,
            trigger_prefixes: vec!['/', '#'],
        }
    }

    /// Does `ch` look like the start of a trigger?
    pub fn is_trigger_prefix(&self, ch: char) -> bool {
        self.trigger_prefixes.contains(&ch)
    }

    /// Walk the trie one character from `current_path`. `current_path` is the string
    /// of characters already matched (empty on the first call of a trigger sequence).
    pub fn step(&self, current_path: &str, ch: char) -> Match {
        let mut node = &self.root;
        // Replay the path to get to the current position.
        let full: String = format!("{current_path}{ch}");
        for c in full.chars() {
            match node.children.get(&c) {
                Some(n) => node = n,
                None => return Match::None,
            }
        }
        // Now `node` is at the position after processing the full path.
        if let (Some(expansion), Some(trigger)) = (&node.expansion, &node.trigger) {
            // Only complete if there are NO further children that could extend this match.
            // This implements longest-prefix: "/g" won't match if "/greet" also exists
            // and the user hasn't typed the characters to disambiguate yet.
            if node.children.is_empty() {
                Match::Complete {
                    trigger: trigger.clone(),
                    expansion: expansion.clone(),
                }
            } else {
                Match::Partial
            }
        } else if node.children.is_empty() {
            Match::None
        } else {
            Match::Partial
        }
    }

    /// Try to match the complete accumulated buffer at once. Used for triggers that
    /// are typed then committed immediately (special commands like `#asr`).
    pub fn match_exact(&self, buffer: &str) -> Option<String> {
        let mut node = &self.root;
        for ch in buffer.chars() {
            node = node.children.get(&ch)?;
        }
        node.expansion.clone()
    }

    /// Number of triggers in the trie (for diagnostics).
    pub fn len(&self) -> usize {
        self.count_leaves(&self.root)
    }

    pub fn is_empty(&self) -> bool {
        self.root.children.is_empty()
    }

    fn count_leaves(&self, node: &Node) -> usize {
        let mut count = if node.expansion.is_some() { 1 } else { 0 };
        for child in node.children.values() {
            count += self.count_leaves(child);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_matcher() -> Matcher {
        Matcher::new(vec![
            ("/greet".into(), "你好,我是 AI 秘书".into()),
            ("/sig".into(), "Best regards,\nAlice".into()),
            ("#asr".into(), "__ASR_BUFFER__".into()),
            ("#exec_resize".into(), "__EXEC_resize__".into()),
        ])
    }

    #[test]
    fn trigger_prefix_detection() {
        let m = test_matcher();
        assert!(m.is_trigger_prefix('/'));
        assert!(m.is_trigger_prefix('#'));
        assert!(!m.is_trigger_prefix('a'));
        assert!(!m.is_trigger_prefix(' '));
    }

    #[test]
    fn complete_match_step_by_step() {
        let m = test_matcher();
        assert_eq!(m.step("", '/'), Match::Partial);
        assert_eq!(m.step("/", 'g'), Match::Partial);
        assert_eq!(m.step("/g", 'r'), Match::Partial);
        assert_eq!(m.step("/gr", 'e'), Match::Partial);
        assert_eq!(m.step("/gre", 'e'), Match::Partial);
        // "/greet" has no children beyond 't' — complete.
        let r = m.step("/gree", 't');
        assert_eq!(
            r,
            Match::Complete {
                trigger: "/greet".into(),
                expansion: "你好,我是 AI 秘书".into()
            }
        );
    }

    #[test]
    fn partial_returns_partial_while_ambiguous() {
        let m = test_matcher();
        assert_eq!(m.step("", '/'), Match::Partial);
    }

    #[test]
    fn dead_end_returns_none() {
        let m = test_matcher();
        assert_eq!(m.step("", 'x'), Match::None);
        assert_eq!(m.step("/g", 'x'), Match::None);
    }

    #[test]
    fn exact_match_for_special_triggers() {
        let m = test_matcher();
        assert_eq!(m.match_exact("#asr"), Some("__ASR_BUFFER__".into()));
        assert_eq!(m.match_exact("#exec_resize"), Some("__EXEC_resize__".into()));
        assert_eq!(m.match_exact("/greet"), Some("你好,我是 AI 秘书".into()));
        assert_eq!(m.match_exact("#unknown"), None);
    }

    #[test]
    fn empty_matcher_all_none() {
        let m = Matcher::new(vec![]);
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert_eq!(m.step("", '/'), Match::None);
    }

    #[test]
    fn matcher_len_counts_unique_triggers() {
        assert_eq!(test_matcher().len(), 4);
    }
}
