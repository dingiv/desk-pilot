//! WeightStore — SQLite-backed persistence for user weight data.
//!
//! Replaces fragmented JSON files (L0 + Bigram + PhraseBook) with a single
//! embedded database at `~/.desk-pilot/swift-ime.db`.
//!
//! Tables:
//! - `bigrams`: (prev_word, next_word) → occurrence count
//! - `pins`:    pinyin → preferred word
//! - `phrases`: (pinyin, word) → priority order
//! - `recency`: recent-commit ring (pos 0 = most recent) — full-snapshot
//!   replaced on every commit (≤64 rows, one transaction)
//! - `l0`:      inputx-pinyin L0 user model (single-row JSON)

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
        // 迁移:老库的 phrases 表没有 count 列(SQLite 的 ADD COLUMN 无
        // IF NOT EXISTS —— 已存在时报错,忽略即可)。
        let _ = conn.execute("ALTER TABLE phrases ADD COLUMN count INTEGER NOT NULL DEFAULT 1", []);
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
                count    INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (pinyin, word)
            );
            CREATE TABLE IF NOT EXISTS recency (
                pos  INTEGER NOT NULL PRIMARY KEY,
                word TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS l0 (
                id   INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
                json TEXT NOT NULL
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

    /// Export all bigrams as a vec of (prev, next, count).
    /// Used to warm the in-memory UserBigram on startup.
    pub fn load_all_bigrams(&self) -> Vec<(String, String, u32)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT prev, next, count FROM bigrams ORDER BY count DESC") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get(2)?))
        })
        .ok()
        .into_iter()
        .flat_map(|rows| rows.filter_map(|r| r.ok()))
        .collect()
    }

    /// Get max bigram count (internal).
    pub fn max_bigram_count(&self) -> u32 {
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

    /// Record a user-learned phrase with priority (0 = highest), count starts at 1.
    pub fn record_phrase(&self, pinyin: &str, word: &str, priority: i32) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO phrases (pinyin, word, priority, count) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(pinyin, word) DO UPDATE SET priority = MIN(priority, ?3)",
            params![pinyin, word, priority],
        );
    }

    /// 用户再次选中已学短语:使用次数 +1(参与 phrase 排名)。
    pub fn bump_phrase_count(&self, pinyin: &str, word: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE phrases SET count = count + 1 WHERE pinyin = ?1 AND word = ?2",
            params![pinyin, word],
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

    /// Load all user-learned phrases for startup warm — (pinyin, word, priority, count).
    pub fn load_all_phrases(&self) -> Vec<(String, String, i32, u32)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT pinyin, word, priority, count FROM phrases ORDER BY pinyin, priority") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get(2)?,
                row.get::<_, u32>(3).unwrap_or(1), // 老库无 count 列时容错
            ))
        })
        .ok()
        .into_iter()
        .flat_map(|rows| rows.filter_map(|r| r.ok()))
        .collect()
    }

    // ── Recency ─────────────────────────────────────────────────────────

    /// Persist the recency ring as a full snapshot — `words` in most-recent-first
    /// order (RecencyStore::dump). Replaced wholesale on every commit: ≤64 rows
    /// in one transaction, and the pos column preserves the boost-decay order
    /// exactly (pos 0 = the 0.20 boost slot).
    pub fn save_recency(&self, words: &[String]) {
        if words.is_empty() {
            self.clear_recency();
            return;
        }
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM recency", []);
        let mut stmt = match conn.prepare("INSERT INTO recency (pos, word) VALUES (?1, ?2)") {
            Ok(s) => s,
            Err(_) => return,
        };
        for (pos, w) in words.iter().enumerate() {
            let _ = stmt.execute(params![pos as i64, w]);
        }
    }

    /// Load the persisted recency ring, most-recent-first (pos 0 = newest).
    pub fn load_recency(&self) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT word FROM recency ORDER BY pos") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |row| row.get::<_, String>(0))
            .ok()
            .into_iter()
            .flat_map(|rows| rows.filter_map(|r| r.ok()))
            .collect()
    }

    /// Drop the ring (used when the in-memory store is empty).
    pub fn clear_recency(&self) {
        let _ = self.conn.lock().unwrap().execute("DELETE FROM recency", []);
    }

    // ── L0 user model ───────────────────────────────────────────────────

    /// Persist the inputx-pinyin L0 user model (pins + pick counters) as JSON.
    /// Single-row table, upserted on every pick.
    pub fn save_l0(&self, json: &str) {
        if json.is_empty() { return; }
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO l0 (id, json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET json = ?1",
            params![json],
        );
    }

    /// Load the persisted L0 model JSON, if any.
    pub fn load_l0(&self) -> Option<String> {
        self.conn.lock().unwrap()
            .query_row("SELECT json FROM l0 WHERE id = 1", [], |r| r.get(0))
            .ok()
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

    #[test]
    fn load_all_bigrams_roundtrip() {
        let s = temp_store();
        s.record_bigram("大", "陆");
        s.record_bigram("大", "陆");
        s.record_bigram("大", "路");
        s.record_bigram("中", "国");
        let all = s.load_all_bigrams();
        assert_eq!(all.len(), 3);
        // Find the 大陆 entry.
        let dalu = all.iter().find(|(p, n, _)| p == "大" && n == "陆").expect("大陆 not found");
        assert_eq!(dalu.2, 2, "大陆 count should be 2, got {}", dalu.2);
    }

    #[test]
    fn recency_snapshot_roundtrip_preserves_order() {
        let s = temp_store();
        let words: Vec<String> = vec!["最新".into(), "次新".into(), "旧".into()];
        s.save_recency(&words);
        assert_eq!(s.load_recency(), words, "most-recent-first order preserved");

        // Replacement semantics: a newer snapshot fully replaces the old.
        s.save_recency(&["另一个".into()]);
        assert_eq!(s.load_recency(), vec!["另一个".to_string()]);
        s.save_recency(&[]);
        assert!(s.load_recency().is_empty(), "empty snapshot clears the ring");
    }

    #[test]
    fn l0_upsert_and_load() {
        let s = temp_store();
        assert_eq!(s.load_l0(), None, "no L0 before first save");
        s.save_l0(r#"[["n","你",3]]"#);
        assert_eq!(s.load_l0().as_deref(), Some(r#"[["n","你",3]]"#));
        // Upsert: a newer model replaces the old one.
        s.save_l0(r#"[["n","你",4]]"#);
        assert_eq!(s.load_l0().as_deref(), Some(r#"[["n","你",4]]"#));
        // Empty JSON is ignored (never corrupts the stored model).
        s.save_l0("");
        assert_eq!(s.load_l0().as_deref(), Some(r#"[["n","你",4]]"#));
    }
}
