//! Capability interfaces — what Stage3 (or the desktop-pet secretary) CAN do. No scheduling here.

use std::sync::{Arc, Mutex};

use anyhow::Result;

/// Manage correction hotwords. Adds feed back into Stage2's LLM-layer hotword prompt on the very
/// next calibration (via the shared `Arc<Mutex<Vec<String>>>` store).
///
/// NOTE: feeding a NEW hotword back into Stage1's ASR layer (the streaming Zipformer) is NOT
/// supported at runtime — sherpa-onnx bakes hotwords at `OnlineRecognizer` creation. TODO: rebuild
/// the recognizer (or use per-stream hotwords) when the hotword set changes materially.
pub trait HotwordManager: Send + Sync {
    /// Add `word`. Returns true if it was newly added (case-insensitive dedup).
    fn add(&self, word: &str) -> bool;
    /// Remove `word`. Returns true if it was present.
    fn remove(&self, word: &str) -> bool;
    /// Snapshot of the current hotword list.
    fn list(&self) -> Vec<String>;
}

/// A user-supplied correction (raw ASR → correct text), the raw material for fine-tuning / hotword
/// inference. `context` is optional surrounding dialogue.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CorrectionSample {
    pub raw: String,
    pub corrected: String,
    pub context: Option<String>,
}

/// Handle to an (async) fine-tuning job. Opaque id for status polling.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FineTuneHandle {
    pub id: String,
}

/// Trigger dynamic fine-tuning (LoRA) from accumulated corrections. STUB this round.
pub trait FineTuner: Send + Sync {
    fn fine_tune(&self, samples: &[CorrectionSample]) -> Result<FineTuneHandle>;
}

/// Condense a rolling context window into a long-term summary. STUB this round.
pub trait ContextSummarizer: Send + Sync {
    fn summarize(&self, context: &str) -> Result<String>;
}

/// Long-term key/value memory across sessions. STUB this round.
pub trait MemoryStore: Send + Sync {
    fn store(&self, key: &str, value: &str) -> Result<()>;
    fn recall(&self, query: &str) -> Result<Vec<String>>;
}

// ── implementations ─────────────────────────────────────────────────────────────

/// `HotwordManager` over a shared `Arc<Mutex<Vec<String>>>` — the same store Stage2's calibrator
/// reads, so an add is visible on the next calibration. Build it from the daemon with the Arc the
/// calibrator also holds.
#[derive(Clone)]
pub struct SharedHotwordManager {
    words: Arc<Mutex<Vec<String>>>,
}

impl SharedHotwordManager {
    pub fn new(words: Arc<Mutex<Vec<String>>>) -> Self {
        Self { words }
    }
    /// The backing store (clone the Arc to hand the same store to Stage2's calibrator).
    pub fn store(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.words)
    }
}

impl HotwordManager for SharedHotwordManager {
    fn add(&self, word: &str) -> bool {
        let w = word.trim();
        if w.is_empty() {
            return false;
        }
        let mut g = self.words.lock().unwrap();
        if g.iter().any(|x| x.eq_ignore_ascii_case(w)) {
            return false;
        }
        g.push(w.to_string());
        true
    }
    fn remove(&self, word: &str) -> bool {
        let mut g = self.words.lock().unwrap();
        let before = g.len();
        g.retain(|x| !x.eq_ignore_ascii_case(word.trim()));
        g.len() != before
    }
    fn list(&self) -> Vec<String> {
        self.words.lock().unwrap().clone()
    }
}

/// No-op FineTuner (returns a synthetic handle; real LoRA later).
#[derive(Default)]
pub struct StubFineTuner;
impl FineTuner for StubFineTuner {
    fn fine_tune(&self, _samples: &[CorrectionSample]) -> Result<FineTuneHandle> {
        Ok(FineTuneHandle { id: "stub-noop".into() })
    }
}

/// No-op ContextSummarizer (returns the input unchanged).
#[derive(Default)]
pub struct StubContextSummarizer;
impl ContextSummarizer for StubContextSummarizer {
    fn summarize(&self, context: &str) -> Result<String> {
        Ok(context.to_string())
    }
}

/// No-op MemoryStore (in-process map; not persisted).
#[derive(Default)]
pub struct StubMemoryStore {
    map: Mutex<Vec<(String, String)>>,
}
impl MemoryStore for StubMemoryStore {
    fn store(&self, key: &str, value: &str) -> Result<()> {
        self.map.lock().unwrap().push((key.to_string(), value.to_string()));
        Ok(())
    }
    fn recall(&self, query: &str) -> Result<Vec<String>> {
        let q = query.to_lowercase();
        Ok(self
            .map
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.to_lowercase().contains(&q))
            .map(|(_, v)| v.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotword_add_dedups_case_insensitively() {
        let store = Arc::new(Mutex::new(vec!["Rust".into()]));
        let mgr = SharedHotwordManager::new(Arc::clone(&store));
        assert!(!mgr.add("rust"), "case-insensitive dedup");
        assert!(!mgr.add("Rust"), "exact dedup");
        assert!(mgr.add("Bevy"));
        assert_eq!(mgr.list(), vec!["Rust".to_string(), "Bevy".to_string()]);
        assert!(mgr.remove("bevy"));
        assert_eq!(mgr.list(), vec!["Rust".to_string()]);
    }

    #[test]
    fn add_refuses_empty() {
        let mgr = SharedHotwordManager::new(Arc::new(Mutex::new(vec![])));
        assert!(!mgr.add("   "));
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn store_shared_with_outside_reader() {
        // The daemon hands the SAME store to SharedHotwordManager and Stage2's calibrator.
        let store = Arc::new(Mutex::new(vec![]));
        let mgr = SharedHotwordManager::new(Arc::clone(&store));
        mgr.add("Rust");
        // outside reader (Stage2) sees it immediately
        assert_eq!(store.lock().unwrap().clone(), vec!["Rust".to_string()]);
    }
}
