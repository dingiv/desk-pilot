//! WeightStore — SQLite-backed persistence for user weight data.
//!
//! Replaces fragmented JSON files (L0 + Bigram + PhraseBook) with a single
//! embedded database at `~/.desk-pilot/swift-ime.db`.
//!
//! Tables:
//! - `bigrams`: (prev_word, next_word) → occurrence count
//! - `pins`:    pinyin → preferred word
//! - `phrases`: (pinyin, word) → priority order
//! - `recency`: recent-member table — word → last-used wall-clock ms (unix
//!   epoch). Full-snapshot replaced on every commit (≤512 rows, one
//!   transaction); the 3-day window is the store's own eviction rule.
//! - `en_user`: 英文自生词 word → 使用次数(Enter 强选 raw 文本时学习)
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
        // 迁移 1:老库的 phrases 表没有 count 列(SQLite 的 ADD COLUMN 无
        // IF NOT EXISTS —— 已存在时报错,忽略即可)。
        let _ = conn.execute("ALTER TABLE phrases ADD COLUMN count INTEGER NOT NULL DEFAULT 1", []);
        // 迁移 2:老格式的 recency (pos, word) 无时间戳 → 重建为 (word, used_at)。
        // 只在旧结构存在时 DROP —— 每次 open 都删会清掉刚写入的数据。
        let has_used_at: bool = conn
            .prepare("PRAGMA table_info(recency)")
            .map(|mut stmt| {
                let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();
                cols.iter().any(|c| c == "used_at")
            })
            .unwrap_or(false);
        if !has_used_at {
            let _ = conn.execute("DROP TABLE IF EXISTS recency", []);
        }
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
                word     TEXT NOT NULL PRIMARY KEY,
                used_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS en_user (
                word  TEXT NOT NULL PRIMARY KEY,
                count INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS l0 (
                id   INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
                json TEXT NOT NULL
            );"
        )?;
        Ok(WeightStore { conn: Mutex::new(conn) })
    }

    // ── 计数(启动日志)────────────────────────────────────────────────

    /// 学习词条数。
    pub fn phrase_count(&self) -> usize {
        self.conn.lock().unwrap()
            .query_row("SELECT COUNT(*) FROM phrases", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// 英文自生词数。
    pub fn en_user_count(&self) -> usize {
        self.conn.lock().unwrap()
            .query_row("SELECT COUNT(*) FROM en_user", [], |r| r.get(0))
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

    // ── Recency (recent member) ─────────────────────────────────────────

    /// Persist the recent table as a full snapshot — `(word, last_used_ms)`
    /// pairs. Replaced wholesale on every commit (≤512 rows, one transaction);
    /// the 3-day window is the store's own eviction rule.
    pub fn save_recency(&self, entries: &[(String, i64)]) {
        if entries.is_empty() {
            self.clear_recency();
            return;
        }
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM recency", []);
        let mut stmt = match conn.prepare("INSERT INTO recency (word, used_at) VALUES (?1, ?2)") {
            Ok(s) => s,
            Err(_) => return,
        };
        for (w, t) in entries {
            let _ = stmt.execute(params![w, t]);
        }
    }

    /// Load the persisted recent entries as `(word, last_used_ms)`.
    pub fn load_recency(&self) -> Vec<(String, i64)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT word, used_at FROM recency") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .ok()
            .into_iter()
            .flat_map(|rows| rows.filter_map(|r| r.ok()))
            .collect()
    }

    /// Drop the table (used when the in-memory store is empty).
    pub fn clear_recency(&self) {
        let _ = self.conn.lock().unwrap().execute("DELETE FROM recency", []);
    }

    // ── 英文自生词 ──────────────────────────────────────────────────────

    /// 学习/递增一个英文自生词(Enter 强制提交 raw 文本,如 cd)。
    pub fn record_en_user(&self, word: &str) {
        if word.is_empty() { return; }
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO en_user (word, count) VALUES (?1, 1)
             ON CONFLICT(word) DO UPDATE SET count = count + 1",
            params![word],
        );
    }

    /// 全部英文自生词 → (word, count),启动 warm 用。
    pub fn load_all_en_user(&self) -> Vec<(String, u32)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT word, count FROM en_user") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)))
            .ok()
            .into_iter()
            .flat_map(|rows| rows.filter_map(|r| r.ok()))
            .collect()
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
    fn phrase_priority() {
        let s = temp_store();
        s.record_phrase("ceshi", "测试", 0);
        s.record_phrase("ceshi", "侧室", 1);
        let phrases = s.phrases_for("ceshi");
        assert_eq!(phrases[0].0, "测试"); // priority 0 first
    }

    #[test]
    fn recency_snapshot_roundtrip_preserves_timestamps() {
        let s = temp_store();
        let entries: Vec<(String, i64)> =
            vec![("最新".into(), 1000), ("次新".into(), 2000), ("旧".into(), 3000)];
        s.save_recency(&entries);
        assert_eq!(s.load_recency(), entries, "word + timestamp preserved");

        // Replacement semantics: a newer snapshot fully replaces the old.
        s.save_recency(&[("另一个".into(), 4000)]);
        assert_eq!(s.load_recency(), vec![("另一个".to_string(), 4000)]);
        s.save_recency(&[]);
        assert!(s.load_recency().is_empty(), "empty snapshot clears the table");
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
