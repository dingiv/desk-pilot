//!
//! Owns the single SQLite connection and coordinates EVERY user-model
//! persistence path: recency ring, bigrams, pins, phrases, and the inputx-pinyin
//! L0 user model. The engine holds one `PersistenceManager`; startup calls
//! [`warm_all`](PersistenceManager::warm_all) to load all persisted state into
//! the in-memory stores in one place, and the families double-write inline
//! (they hold the same `Arc<WeightStore>` via `attach_store`).
//!
//! ```
//! use ime_core::store::PersistenceManager;
//! // engine startup: open once, warm everything
//! let pm = PersistenceManager::open("/tmp/swift-ime-docex.db")?;
//! // pm.warm_all(&dispatcher);  // the engine does this in init_store
//! let store = pm.store();
//! # Ok::<(), rusqlite::Error>(())
//! ```

use std::sync::Arc;

use super::sqlite::WeightStore;
use crate::dispatcher::Dispatcher;

/// Unified persistence manager — the engine's single handle to the SQLite
/// store. Clone is cheap (shared connection behind an `Arc`).
#[derive(Clone)]
pub struct PersistenceManager {
    store: Arc<WeightStore>,
}

impl PersistenceManager {
    /// Open (or create) the user database — schema migration happens here.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        Ok(PersistenceManager {
            store: Arc::new(WeightStore::open(path)?),
        })
    }

    /// The underlying store — families hold this Arc for inline double-writes.
    pub fn store(&self) -> Arc<WeightStore> {
        Arc::clone(&self.store)
    }

    /// Startup warm: load EVERY persisted user model into the in-memory stores.
    /// Order matters — `set_store` must come first so the families' double-write
    /// path is armed before any warm reads.
    pub fn warm_all(&self, disp: &Dispatcher) {
        // Families double-write through this Arc (recency / L0 / phrases).
        disp.set_store(self.store());

        // Phrases → PhraseBook.
        disp.warm_phrases_from_store();

        // 英文自生词 → EnglishFamily user 层。
        let en_user = self.store.load_all_en_user();
        if !en_user.is_empty() {
            disp.warm_en_user(en_user);
            eprintln!("[ime-core] english: warmed learned words");
        }

        // Recency ring (most-recent-first; the family reverses for load_bulk).
        let recency = self.store.load_recency();
        if !recency.is_empty() {
            disp.warm_recencies(recency);
        }

        // L0 user model (pins + pick counters) → inputx-pinyin.
        if let Some(json) = self.store.load_l0() {
            let pins = disp.import_l0(&json);
            if pins > 0 {
                eprintln!("[ime-core] pinyin: restored {pins} L0 pins from store");
            }
        }
    }

    // ── Forwarding accessors (the engine's persistence surface) ─────────

    /// 学习词条数(启动日志用)。
    pub fn phrase_count(&self) -> usize {
        self.store.phrase_count()
    }

    /// 英文自生词数(启动日志用)。
    pub fn en_user_count(&self) -> usize {
        self.store.en_user_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("/tmp/swift-ime-pm-{}-{}.db", std::process::id(), id)
    }

    #[test]
    fn open_creates_schema_and_roundtrips() {
        let path = temp_path();
        let pm = PersistenceManager::open(&path).expect("open");
        // All five tables exist (schema migration ran).
        let store = pm.store();
        store.save_recency(&[("a".into(), 1000), ("b".into(), 2000)]);
        store.record_phrase("ceshi", "测试", 0);
        store.save_l0(r#"{"pins":[],"picks":[]}"#);

        let pm2 = PersistenceManager::open(&path).expect("reopen");
        assert_eq!(
            pm2.store().load_recency(),
            vec![("a".to_string(), 1000), ("b".to_string(), 2000)]
        );
        assert_eq!(pm2.store().load_all_phrases().len(), 1);
        assert_eq!(
            pm2.store().load_l0().as_deref(),
            Some(r#"{"pins":[],"picks":[]}"#)
        );
    }

    #[test]
    fn forwarding_accessors_work() {
        let pm = PersistenceManager::open(&temp_path()).expect("open");
        pm.store().record_phrase("ceshi", "测试", 0);
        assert_eq!(pm.phrase_count(), 1);
        pm.store().record_en_user("cd");
        assert_eq!(pm.en_user_count(), 1);
    }
}
