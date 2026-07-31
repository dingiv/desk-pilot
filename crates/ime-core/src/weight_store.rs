//! WeightStore — SQLite-backed persistence for user weight data.
//!
//! Replaces fragmented JSON files (L0 + Bigram + PhraseBook) with a single
//! embedded database at `~/.desk-pilot/swift-ime.db`.
//!
//! Tables:
//! - `bigrams`: (prev_word, next_word) → occurrence count
//! - `pins`:    pinyin → preferred word
//! - `phrases`: (pinyin, word) → priority order

use rusqlite::{Connection, params};
use std::sync::Mutex;

pub struct WeightStore {
    conn: Mutex<Connection>,
}

impl WeightStore {
    /// Open (or create) the database at `path`, auto-migrating the schema.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bigrams (
                prev  TEXT NOT NULL,
                next  TEXT NOT NULL,
                count INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (prev, next)
            );
            CREATE TABLE IF NOT EXISTS pins (
                pinyin    TEXT NOT NULL PRIMARY KEY,
                word      TEXT NOT NULL,
                pinned_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE TABLE IF NOT EXISTS phrases (
                pinyin   TEXT NOT NULL,
                word     TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (pinyin, word)
            );"
        )?;
        Ok(WeightStore { conn: Mutex::new(conn) })
    }

    // ── Bigrams ─────────────────────────────────────────────────────────

    /// Record a bigram occurrence. `prev` → `next` was observed in user input.
    pub fn record_bigram(&self, prev: &str, next: &str) {
        if prev.is_empty() || next.is_empty() { return; }
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO bigrams (prev, next, count) VALUES (?1, ?2, 1)
             ON CONFLICT(prev, next) DO UPDATE SET count = count + 1",
            params![prev, next],
        );
    }

    /// Get the boost factor for a candidate given context words.
    /// Returns 1.0 + up to 0.25 based on bigram frequency.
    pub fn bigram_boost(&self, context_words: &[String], candidate: &str) -> f64 {
        if context_words.is_empty() || candidate.is_empty() { return 1.0; }
        let (total, max) = {
            let conn = self.conn.lock().unwrap();
            let placeholders: Vec<String> = (0..context_words.len()).map(|i| format!("?{}", i + 2)).collect();
            let sql = format!(
                "SELECT COALESCE(SUM(count),0) FROM bigrams WHERE next = ?1 AND prev IN ({})",
                placeholders.join(",")
            );
            let total: u32 = (|| {
                let mut stmt = conn.prepare(&sql).ok()?;
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(candidate.to_string())];
                for w in context_words { params.push(Box::new(w.clone())); }
                stmt.query_row(rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())), |row| row.get(0)).ok()
            })().unwrap_or(0);
            let max: u32 = conn.query_row("SELECT COALESCE(MAX(count), 1) FROM bigrams", [], |r| r.get(0)).unwrap_or(1);
            (total, max)
        };
        if total == 0 { return 1.0; }
        1.0 + (total as f64 / max.max(1) as f64) * 0.25
    }

    /// Get max bigram count (internal, caller must NOT hold the lock).
    #[allow(dead_code)]
    fn max_bigram_count(&self) -> u32 {
        self.conn.lock().unwrap()
            .query_row("SELECT COALESCE(MAX(count), 1) FROM bigrams", [], |r| r.get(0))
            .unwrap_or(1)
    }

    // ── Pins ────────────────────────────────────────────────────────────

    /// Pin a word for a pinyin (0 = highest priority, like fcitx5).
    pub fn pin_word(&self, pinyin: &str, word: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO pins (pinyin, word) VALUES (?1, ?2)
             ON CONFLICT(pinyin) DO UPDATE SET word = ?2, pinned_at = unixepoch()",
            params![pinyin, word],
        );
    }

    /// Get the pinned word for a pinyin, if any.
    pub fn pinned_word(&self, pinyin: &str) -> Option<String> {
        self.conn.lock().unwrap()
            .query_row("SELECT word FROM pins WHERE pinyin = ?1", params![pinyin], |r| r.get(0))
            .ok()
    }

    /// Count of pinned words (for diagnostics).
    pub fn pin_count(&self) -> usize {
        self.conn.lock().unwrap()
            .query_row("SELECT COUNT(*) FROM pins", [], |r| r.get(0))
            .unwrap_or(0)
    }

    // ── Phrases ─────────────────────────────────────────────────────────

    /// Record a user-learned phrase with priority (0 = highest).
    pub fn record_phrase(&self, pinyin: &str, word: &str, priority: i32) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO phrases (pinyin, word, priority) VALUES (?1, ?2, ?3)
             ON CONFLICT(pinyin, word) DO UPDATE SET priority = MIN(priority, ?3)",
            params![pinyin, word, priority],
        );
    }

    /// Get phrases for a pinyin, sorted by priority ascending (0 first).
    pub fn phrases_for(&self, pinyin: &str) -> Vec<(String, i32)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT word, priority FROM phrases WHERE pinyin = ?1 ORDER BY priority, word"
        ).ok();
        stmt.as_mut().map(|s| {
            s.query_map(params![pinyin], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?)))
                .ok().into_iter().flat_map(|rows| rows.filter_map(|r| r.ok())).collect()
        }).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> WeightStore {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = format!("/tmp/swift-ime-test-{}-{}.db", std::process::id(), id);
        WeightStore::open(&path).unwrap()
    }

    #[test]
    fn bigram_record_and_boost() {
        let s = temp_store();
        s.record_bigram("大", "陆");
        s.record_bigram("大", "陆");
        s.record_bigram("大", "路");
        assert!(s.bigram_boost(&["大".into()], "陆") > s.bigram_boost(&["大".into()], "路"));
    }

    #[test]
    fn pin_and_retrieve() {
        let s = temp_store();
        s.pin_word("jishi", "即使");
        assert_eq!(s.pinned_word("jishi"), Some("即使".into()));
    }

    #[test]
    fn phrase_priority() {
        let s = temp_store();
        s.record_phrase("ceshi", "测试", 0);
        s.record_phrase("ceshi", "侧室", 1);
        let phrases = s.phrases_for("ceshi");
        assert_eq!(phrases[0].0, "测试"); // priority 0 first
    }
}
