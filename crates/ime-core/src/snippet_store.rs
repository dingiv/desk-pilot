//! Hot-reloadable snippet store. Loaded from JSON, atomically replaced at runtime.

use serde::{Deserialize, Serialize};
use tracing::warn;

/// One snippet: a trigger and its expansion text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snippet {
    /// The trigger string (e.g. "/greet", "#asr", "#exec_resize").
    pub trigger: String,
    /// The expansion text (may contain $DATE, $CLIPBOARD, $CURSOR).
    pub expand: String,
    /// Human-readable description (shown in familiar config panel).
    #[serde(default)]
    pub desc: String,
}

/// Error conditions during snippet loading.
#[derive(Debug, Clone)]
pub enum LoadError {
    /// JSON syntax error.
    Parse(String),
    /// Duplicate triggers found.
    DuplicateTrigger(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Parse(msg) => write!(f, "snippet parse error: {msg}"),
            LoadError::DuplicateTrigger(t) => write!(f, "duplicate trigger: {t}"),
        }
    }
}

/// The validated, immutable set of snippets used at runtime.
/// Atomic hot-reload: the store is swapped as a whole (`Arc<SnippetStore>` or
/// simple `replace`), never mutated in-place.
#[derive(Debug, Clone, Default)]
pub struct SnippetStore {
    snippets: Vec<Snippet>,
}nihao

impl SnippetStore {
    /// Create an empty store.
    pub fn new() -> Self {
        SnippetStore { snippets: Vec::new() }
    }

    /// Load snippets from JSON bytes. Validates no duplicate triggers.
    pub fn from_json(json: &str) -> Result<Self, LoadError> {
        let raw: Vec<Snippet> =
            serde_json::from_str(json).map_err(|e| LoadError::Parse(e.to_string()))?;
        Self::from_vec(raw)
    }

    /// Build the store from a pre-parsed vec (used by familiar's config push via socket).
    pub fn from_vec(snippets: Vec<Snippet>) -> Result<Self, LoadError> {
        // Validate: no duplicate triggers.
        let mut seen = std::collections::HashSet::new();
        for s in &snippets {
            if !seen.insert(&s.trigger) {
                return Err(LoadError::DuplicateTrigger(s.trigger.clone()));
            }
        }
        Ok(SnippetStore { snippets })
    }

    /// Replace the entire store atomically. Invalid input (duplicates) is rejected
    /// and logged — the existing store is NOT modified.
    pub fn replace(&mut self, snippets: Vec<Snippet>) {
        match Self::from_vec(snippets) {
            Ok(new_store) => {
                let count = new_store.snippets.len();
                *self = new_store;
                tracing::debug!(snippet_count = count, "snippet store replaced");
            }
            Err(e) => {
                warn!(error = %e, "snippet store replace rejected — keeping existing store");
            }
        }
    }

    /// All (trigger, expansion) pairs for building a Matcher.
    pub fn entries(&self) -> Vec<(String, String)> {
        self.snippets
            .iter()
            .map(|s| (s.trigger.clone(), s.expand.clone()))
            .collect()
    }

    /// All snippets (for serialization / familiar config push).
    pub fn all(&self) -> &[Snippet] {
        &self.snippets
    }

    /// Number of loaded snippets.
    pub fn len(&self) -> usize {
        self.snippets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snippets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_JSON: &str = r##"[
        {"trigger": "/greet", "expand": "Hello!", "desc": "greeting"},
        {"trigger": "/sig", "expand": "Best,\nAlice", "desc": ""},
        {"trigger": "#asr", "expand": "__ASR__"}
    ]"##;

    #[test]
    fn load_valid_json() {
        let store = SnippetStore::from_json(VALID_JSON).unwrap();
        assert_eq!(store.len(), 3);
        assert_eq!(store.all()[0].trigger, "/greet");
    }

    #[test]
    fn reject_duplicate_triggers() {
        let json = r##"[
            {"trigger": "/a", "expand": "first"},
            {"trigger": "/a", "expand": "second"}
        ]"##;
        match SnippetStore::from_json(json) {
            Err(LoadError::DuplicateTrigger(t)) => assert_eq!(t, "/a"),
            other => panic!("expected DuplicateTrigger, got {other:?}"),
        }
    }

    #[test]
    fn reject_bad_json() {
        let json = r##"not json"##;
        match SnippetStore::from_json(json) {
            Err(LoadError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn replace_keeps_old_on_invalid() {
        let mut store = SnippetStore::from_json(VALID_JSON).unwrap();
        let before = store.len();
        // Try to replace with duplicate-ridden data.
        store.replace(vec![
            Snippet { trigger: "/a".into(), expand: "1".into(), desc: "".into() },
            Snippet { trigger: "/a".into(), expand: "2".into(), desc: "".into() },
        ]);
        // Old store untouched.
        assert_eq!(store.len(), before);
        assert_eq!(store.all()[0].trigger, "/greet");
    }

    #[test]
    fn replace_atomic_swap() {
        let mut store = SnippetStore::from_json(VALID_JSON).unwrap();
        store.replace(vec![
            Snippet { trigger: "/new".into(), expand: "new_expand".into(), desc: "".into() },
        ]);
        assert_eq!(store.len(), 1);
        assert_eq!(store.all()[0].trigger, "/new");
    }

    #[test]
    fn empty_store() {
        let store = SnippetStore::new();
        assert!(store.is_empty());
        assert!(store.entries().is_empty());
    }
}
