//! WeightStore — SQLite-backed persistence for user weight data.
//!
//! Replaces fragmented JSON files (L0 + Bigram + PhraseBook) with a single
//! embedded database at `~/.desk-pilot/swift-ime.db`.
//!
//! Tables:
//! - `bigrams`: (prev_word, next_word) → occurrence count
//! - `pins`:    pinyin → preferred word
//! - `phrases`: (pinyin, word) → priority order
//!   epoch). Full-snapshot replaced on every commit (≤512 rows, one
//!   transaction); the 3-day window is the store's own eviction rule.
//! - `en_user`: 英文自生词 word → 使用次数(Enter 强选 raw 文本时学习)
//! - `l0`:      inputx-pinyin L0 user model (single-row JSON)

use rusqlite::{params, Connection};
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
        // schema 同步:round22 前的库文件没有 delta 列(仅补列,不做数据迁移)。
        let _ = conn.execute(
            "ALTER TABLE overlay_freq ADD COLUMN delta INTEGER NOT NULL DEFAULT 0",
            [],
        );
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
            CREATE TABLE IF NOT EXISTS overlay_freq (
                word      TEXT NOT NULL PRIMARY KEY,
                pinyin    TEXT NOT NULL DEFAULT '',
                frequency INTEGER NOT NULL DEFAULT 0,
                count     INTEGER NOT NULL DEFAULT 0,
                delta     INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS overlay_recent (
                word    TEXT NOT NULL PRIMARY KEY,
                last_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS en_user (
                word  TEXT NOT NULL PRIMARY KEY,
                count INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS l0 (
                id   INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
                json TEXT NOT NULL
            );",
        )?;
        Ok(WeightStore {
            conn: Mutex::new(conn),
        })
    }

    // ── 计数(启动日志)────────────────────────────────────────────────

    /// 学习词条数。
    pub fn phrase_count(&self) -> usize {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM phrases", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// 英文自生词数。
    pub fn en_user_count(&self) -> usize {
        self.conn
            .lock()
            .unwrap()
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

    /// 删除一条自生词(污染清理用)。
    pub fn delete_phrase(&self, pinyin: &str, word: &str) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM phrases WHERE pinyin = ?1 AND word = ?2",
            params![pinyin, word],
        )
        .unwrap_or(0)
    }

    /// Get phrases for a pinyin, sorted by priority ascending (0 first).
    pub fn phrases_for(&self, pinyin: &str) -> Vec<(String, i32)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT word, priority FROM phrases WHERE pinyin = ?1 ORDER BY priority, word")
            .ok();
        stmt.as_mut()
            .map(|s| {
                s.query_map(params![pinyin], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?))
                })
                .ok()
                .into_iter()
                .flat_map(|rows| rows.filter_map(|r| r.ok()))
                .collect()
            })
            .unwrap_or_default()
    }

    /// Load all user-learned phrases for startup warm — (pinyin, word, priority, count).
    pub fn load_all_phrases(&self) -> Vec<(String, String, i32, u32)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT pinyin, word, priority, count FROM phrases ORDER BY pinyin, priority")
        {
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

    // ── L2 OverlayDict(round19 三级架构;分表:频率/时间)──────────────

    /// Save the L2 frequency table(全量快照替换;行含 delta 账本)。
    pub fn save_overlay_freq(&self, rows: &[(String, String, u64, i64, u32)]) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM overlay_freq", []);
        let mut stmt = match conn.prepare(
            "INSERT INTO overlay_freq (word, pinyin, frequency, delta, count) VALUES (?1, ?2, ?3, ?4, ?5)",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        for (w, p, f, d, c) in rows {
            let _ = stmt.execute(params![w, p, f, d, c]);
        }
    }

    /// Load the L2 frequency table(行 = word, pinyin, base, delta, count)。
    pub fn load_overlay_freq(&self) -> Vec<(String, String, u64, i64, u32)> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut stmt) = conn
            .prepare("SELECT word, pinyin, frequency, delta, count FROM overlay_freq")
        else {
            return Vec::new();
        };
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, u32>(4)?,
            ))
        })
        .ok()
        .into_iter()
        .flat_map(|rows| rows.filter_map(|r| r.ok()))
        .collect()
    }

    /// Save the L2 time table(全量快照替换)。
    pub fn save_overlay_recent(&self, rows: &[(String, i64)]) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM overlay_recent", []);
        let mut stmt = match conn
            .prepare("INSERT INTO overlay_recent (word, last_ms) VALUES (?1, ?2)")
        {
            Ok(s) => s,
            Err(_) => return,
        };
        for (w, t) in rows {
            let _ = stmt.execute(params![w, t]);
        }
    }

    /// Load the L2 time table。
    pub fn load_overlay_recent(&self) -> Vec<(String, i64)> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut stmt) = conn.prepare("SELECT word, last_ms FROM overlay_recent") else {
            return Vec::new();
        };
        stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .ok()
            .into_iter()
            .flat_map(|rows| rows.filter_map(|r| r.ok()))
            .collect()
    }

    // ── 英文自生词 ──────────────────────────────────────────────────────

    /// 学习/递增一个英文自生词(Enter 强制提交 raw 文本,如 cd)。
    pub fn record_en_user(&self, word: &str) {
        if word.is_empty() {
            return;
        }
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
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })
        .ok()
        .into_iter()
        .flat_map(|rows| rows.filter_map(|r| r.ok()))
        .collect()
    }

    // ── L0 user model ───────────────────────────────────────────────────

    /// Persist the inputx-pinyin L0 user model (pins + pick counters) as JSON.
    /// Single-row table, upserted on every pick.
    pub fn save_l0(&self, json: &str) {
        if json.is_empty() {
            return;
        }
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO l0 (id, json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET json = ?1",
            params![json],
        );
    }

    /// Load the persisted L0 model JSON, if any.
    pub fn load_l0(&self) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
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
