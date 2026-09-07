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
//! let pm = PersistenceManager::open_with_wordbook(
//!     "/tmp/swift-ime-docex.db",
//!     std::sync::Arc::new(ime_core::store::wordbook::WordBook::default()),
//! )?;
//! // pm.warm_all(&dispatcher);  // the engine does this in init_store
//! let store = pm.store();
//! # Ok::<(), rusqlite::Error>(())
//! ```

use std::sync::Arc;

use super::sqlite::WeightStore;
use crate::engine::ImeEngine;

/// Unified persistence manager — the engine's single handle to the SQLite
/// store. Clone is cheap (shared connection behind an `Arc`).
#[derive(Clone)]
pub struct PersistenceManager {
    store: Arc<WeightStore>,
    /// 单词本本体(round14:**所有权在此**)——引擎/SessionState/两家族
    /// 只持引用克隆;学习路径经后处理直接写册,双写经内置 store 槽。
    wordbook: Arc<crate::store::wordbook::WordBook>,
}

impl PersistenceManager {
    /// Open (or create) the user database — schema migration happens here.
    /// `wordbook` 的所有权随本调用移入持久化模块。
    pub fn open_with_wordbook(
        path: &str,
        wordbook: Arc<crate::store::wordbook::WordBook>,
    ) -> rusqlite::Result<Self> {
        Ok(PersistenceManager {
            store: Arc::new(WeightStore::open(path)?),
            wordbook,
        })
    }

    /// 单词本句柄(与所有者共享同一 Arc)。
    pub fn wordbook(&self) -> Arc<crate::store::wordbook::WordBook> {
        Arc::clone(&self.wordbook)
    }

    /// The underlying store — families hold this Arc for inline double-writes.
    pub fn store(&self) -> Arc<WeightStore> {
        Arc::clone(&self.store)
    }

    /// Startup warm: load EVERY persisted user model into the in-memory stores.
    /// Order matters — `set_store` must come first so the families' double-write
    /// path is armed before any warm reads.
    pub fn warm_all(&self, eng: &ImeEngine) {
        // Families double-write through this Arc (recency / L0 / phrases).
        eng.set_store(self.store());

        // Phrases → PhraseBook.
        eng.warm_phrases_from_store();

        // 英文自生词 → EnglishFamily user 层。
        let en_user = self.store.load_all_en_user();
        if !en_user.is_empty() {
            eng.warm_en_user(en_user);
            eprintln!("[ime-core] english: warmed learned words");
        }

        // L2 OverlayDict 冷加载(round19 三级架构)。
        let of = self.store.load_overlay_freq();
        let or = self.store.load_overlay_recent();
        if !of.is_empty() || !or.is_empty() {
            eng.warm_overlay_dict(of, or);
        }

        // L0 user model (pins + pick counters) → inputx-pinyin.
        if let Some(json) = self.store.load_l0() {
            let pins = eng.import_l0(&json);
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
        let pm = PersistenceManager::open_with_wordbook(&path, std::sync::Arc::new(crate::store::wordbook::WordBook::default())).expect("open");
        // Schema + 常驻表往返。
        let store = pm.store();
        store.save_overlay_recent(&[("a".into(), 1000), ("b".into(), 2000)]);
        store.record_phrase("ceshi", "测试", 0);
        store.save_l0(r#"{"pins":[],"picks":[]}"#);

        let pm2 = PersistenceManager::open_with_wordbook(&path, std::sync::Arc::new(crate::store::wordbook::WordBook::default())).expect("reopen");
        assert_eq!(
            pm2.store().load_overlay_recent(),
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
        let pm = PersistenceManager::open_with_wordbook(&temp_path(), std::sync::Arc::new(crate::store::wordbook::WordBook::default())).expect("open");
        pm.store().record_phrase("ceshi", "测试", 0);
        assert_eq!(pm.phrase_count(), 1);
        pm.store().record_en_user("cd");
        assert_eq!(pm.en_user_count(), 1);
    }
}
