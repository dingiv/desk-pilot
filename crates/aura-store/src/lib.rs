//! audio-aura-store — rusqlite storage for the audio-aura core. Same schema + semantics as the TS
//! `src/store.ts` (voice_chunks / calibrated_nodes / topics / tasks), so the daemon and the TS
//! devtools can share `data/voice-agent.db`. Single `Connection` behind a `Mutex` (single-user).

pub mod archive;
pub mod hub;
pub mod wav;

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, Row};
use serde::Serialize;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS topics (
  topic_id TEXT PRIMARY KEY,
  title TEXT NOT NULL DEFAULT 'Untitled',
  article_markdown TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft','generating','complete')),
  created_at INTEGER NOT NULL DEFAULT (unixepoch('subsec')*1000),
  updated_at INTEGER NOT NULL DEFAULT (unixepoch('subsec')*1000)
);
CREATE TABLE IF NOT EXISTS voice_chunks (
  chunk_id TEXT PRIMARY KEY,
  start_time INTEGER NOT NULL,
  end_time INTEGER NOT NULL,
  raw_text TEXT NOT NULL DEFAULT '',
  audio_path TEXT, audio_mime TEXT, duration_ms INTEGER,
  status TEXT NOT NULL DEFAULT 'captured' CHECK (status IN ('captured','calibrated','archived')),
  topic_id TEXT REFERENCES topics(topic_id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch('subsec')*1000)
);
CREATE TABLE IF NOT EXISTS calibrated_nodes (
  node_id TEXT PRIMARY KEY,
  linked_chunks TEXT NOT NULL DEFAULT '[]',
  calibrated_text TEXT NOT NULL DEFAULT '',
  topic_id TEXT REFERENCES topics(topic_id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch('subsec')*1000)
);
CREATE TABLE IF NOT EXISTS tasks (
  task_id TEXT PRIMARY KEY,
  capability TEXT NOT NULL,
  brief TEXT NOT NULL DEFAULT '',
  topic_id TEXT,
  status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','done','failed')),
  result TEXT,
  created_at INTEGER NOT NULL DEFAULT (unixepoch('subsec')*1000),
  updated_at INTEGER NOT NULL DEFAULT (unixepoch('subsec')*1000)
);
CREATE INDEX IF NOT EXISTS idx_chunks_created ON voice_chunks(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_nodes_created ON calibrated_nodes(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_nodes_topic ON calibrated_nodes(topic_id);
CREATE INDEX IF NOT EXISTS idx_topics_updated ON topics(updated_at DESC);
"#;

// ── row types (mirror src/store.ts) ─────────────────────────────────────────────
#[derive(Debug, Clone, Serialize)]
pub struct VoiceChunk {
    pub chunk_id: String,
    pub start_time: i64,
    pub end_time: i64,
    pub raw_text: String,
    pub audio_path: Option<String>,
    pub audio_mime: Option<String>,
    pub duration_ms: Option<i64>,
    pub status: String,
    pub topic_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibratedNode {
    pub node_id: String,
    pub linked_chunks: String,
    pub calibrated_text: String,
    pub topic_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Topic {
    pub topic_id: String,
    pub title: String,
    pub article_markdown: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub task_id: String,
    pub capability: String,
    pub brief: String,
    pub topic_id: Option<String>,
    pub status: String,
    pub result: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct NewChunk<'a> {
    pub chunk_id: &'a str,
    pub start_time: i64,
    pub end_time: i64,
    pub raw_text: &'a str,
    pub audio_path: Option<&'a str>,
    pub audio_mime: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub topic_id: Option<&'a str>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(db_path: &str) -> Result<Self> {
        if db_path != ":memory:" {
            if let Some(dir) = Path::new(db_path).parent() {
                std::fs::create_dir_all(dir).ok();
            }
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(DDL)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    // ── chunks ───────────────────────────────────────────────────────────────
    pub fn create_chunk(&self, c: &NewChunk) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO voice_chunks (chunk_id,start_time,end_time,raw_text,audio_path,audio_mime,duration_ms,topic_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![c.chunk_id, c.start_time, c.end_time, c.raw_text, c.audio_path, c.audio_mime, c.duration_ms, c.topic_id],
        )?;
        Ok(())
    }

    pub fn mark_chunk_calibrated(&self, chunk_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE voice_chunks SET status='calibrated' WHERE chunk_id=?1", [chunk_id])?;
        Ok(())
    }

    pub fn get_chunk(&self, chunk_id: &str) -> Result<Option<VoiceChunk>> {
        let conn = self.conn.lock().unwrap();
        let r = conn
            .query_row("SELECT * FROM voice_chunks WHERE chunk_id=?1", [chunk_id], map_chunk)
            .ok();
        Ok(r)
    }

    // ── nodes ────────────────────────────────────────────────────────────────
    pub fn create_node(&self, node_id: &str, linked_chunks: &[String], calibrated_text: &str, topic_id: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let linked = serde_json::to_string(linked_chunks).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "INSERT INTO calibrated_nodes (node_id,linked_chunks,calibrated_text,topic_id) VALUES (?1,?2,?3,?4)",
            rusqlite::params![node_id, linked, calibrated_text, topic_id],
        )?;
        Ok(())
    }

    pub fn update_node(&self, node_id: &str, calibrated_text: &str, linked_chunks: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let linked = serde_json::to_string(linked_chunks).unwrap_or_else(|_| "[]".into());
        conn.execute(
            "UPDATE calibrated_nodes SET calibrated_text=?1, linked_chunks=?2 WHERE node_id=?3",
            rusqlite::params![calibrated_text, linked, node_id],
        )?;
        Ok(())
    }

    pub fn get_last_node(&self, topic_id: Option<&str>) -> Result<Option<CalibratedNode>> {
        let conn = self.conn.lock().unwrap();
        let r = match topic_id {
            Some(t) => conn.query_row(
                "SELECT * FROM calibrated_nodes WHERE topic_id=?1 ORDER BY created_at DESC LIMIT 1",
                [t], map_node).ok(),
            None => conn.query_row(
                "SELECT * FROM calibrated_nodes ORDER BY created_at DESC LIMIT 1", [], map_node).ok(),
        };
        Ok(r)
    }

    /// Recent nodes newest-first (secretary working memory).
    pub fn get_recent_nodes(&self, limit: i64, topic_id: Option<&str>) -> Result<Vec<CalibratedNode>> {
        let conn = self.conn.lock().unwrap();
        match topic_id {
            Some(t) => {
                let mut stmt = conn.prepare("SELECT * FROM calibrated_nodes WHERE topic_id=?1 ORDER BY created_at DESC LIMIT ?2")?;
                let rows = stmt.query_map(rusqlite::params![t, limit], map_node)?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            }
            None => {
                let mut stmt = conn.prepare("SELECT * FROM calibrated_nodes ORDER BY created_at DESC LIMIT ?1")?;
                let rows = stmt.query_map([limit], map_node)?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            }
        }
    }

    /// All nodes of a topic, oldest-first (writer material).
    pub fn get_nodes_by_topic(&self, topic_id: &str) -> Result<Vec<CalibratedNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM calibrated_nodes WHERE topic_id=?1 ORDER BY created_at ASC")?;
        let rows = stmt.query_map([topic_id], map_node)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    // ── topics ───────────────────────────────────────────────────────────────
    pub fn create_topic(&self, topic_id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO topics (topic_id,title) VALUES (?1,?2)", rusqlite::params![topic_id, title])?;
        Ok(())
    }

    pub fn get_topic(&self, topic_id: &str) -> Result<Option<Topic>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT * FROM topics WHERE topic_id=?1", [topic_id], map_topic).ok())
    }

    pub fn list_topics(&self) -> Result<Vec<Topic>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM topics ORDER BY updated_at DESC")?;
        let rows = stmt.query_map([], map_topic)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn update_topic_title(&self, topic_id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE topics SET title=?1, updated_at=(unixepoch('subsec')*1000) WHERE topic_id=?2",
            rusqlite::params![title, topic_id])?;
        Ok(())
    }

    pub fn set_topic_status(&self, topic_id: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE topics SET status=?1, updated_at=(unixepoch('subsec')*1000) WHERE topic_id=?2",
            rusqlite::params![status, topic_id])?;
        Ok(())
    }

    pub fn set_topic_article(&self, topic_id: &str, markdown: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE topics SET article_markdown=?1, status=?2, updated_at=(unixepoch('subsec')*1000) WHERE topic_id=?3",
            rusqlite::params![markdown, status, topic_id])?;
        Ok(())
    }

    // ── tasks ────────────────────────────────────────────────────────────────
    pub fn create_task(&self, task_id: &str, capability: &str, brief: &str, topic_id: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO tasks (task_id,capability,brief,topic_id) VALUES (?1,?2,?3,?4)",
            rusqlite::params![task_id, capability, brief, topic_id])?;
        Ok(())
    }

    pub fn set_task_status(&self, task_id: &str, status: &str, result: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE tasks SET status=?1, result=COALESCE(?2,result), updated_at=(unixepoch('subsec')*1000) WHERE task_id=?3",
            rusqlite::params![status, result, task_id])?;
        Ok(())
    }
}

// ── row mappers ─────────────────────────────────────────────────────────────
fn map_chunk(r: &Row) -> rusqlite::Result<VoiceChunk> {
    Ok(VoiceChunk {
        chunk_id: r.get("chunk_id")?,
        start_time: r.get("start_time")?,
        end_time: r.get("end_time")?,
        raw_text: r.get("raw_text")?,
        audio_path: r.get("audio_path")?,
        audio_mime: r.get("audio_mime")?,
        duration_ms: r.get("duration_ms")?,
        status: r.get("status")?,
        topic_id: r.get("topic_id")?,
        created_at: r.get("created_at")?,
    })
}
fn map_node(r: &Row) -> rusqlite::Result<CalibratedNode> {
    Ok(CalibratedNode {
        node_id: r.get("node_id")?,
        linked_chunks: r.get("linked_chunks")?,
        calibrated_text: r.get("calibrated_text")?,
        topic_id: r.get("topic_id")?,
        created_at: r.get("created_at")?,
    })
}
fn map_topic(r: &Row) -> rusqlite::Result<Topic> {
    Ok(Topic {
        topic_id: r.get("topic_id")?,
        title: r.get("title")?,
        article_markdown: r.get("article_markdown")?,
        status: r.get("status")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}
