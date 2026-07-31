//! hub — the **Storage 总管**: one facade owning ALL of the daemon's business data.
//!
//! Composes (composition over inheritance — each piece stays single-purpose):
//! - [`AudioArchive`] (`crate::archive`) — utterance PCM: hot replay + date-named WAV flush.
//! - [`TurnLog`] — the Stage1+Stage2 result of every turn, appended as one JSON line to a
//!   date-named file (`<dir>/<YYYY-MM-DD>.jsonl`; rolls over at midnight, greppable, no schema
//!   migration). The clip's WAV path is embedded, linking transcript ↔ audio.
//! - A bounded in-memory ring of recent [`TurnRecord`]s — backs the daemon's `GET /results`.
//!
//! The daemon calls ONE method per finalized utterance ([`Storage::record_final`]); everything
//! else (flush cadence, day rollover, eviction) is the pieces' own business. Future business
//! data (user annotations R4, Stage3 memory) lands here as more composed pieces — the legacy
//! sqlite store (`lib.rs`) stays untouched until M4 decides its fate.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::archive::{AudioArchive, ClipMeta};
use chrono::Local;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// One finalized turn's full Stage1+Stage2 result (what lands in the day log + `/results`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    pub seq: u64,
    /// Wall-clock ms since unix epoch (absolute — day files must be self-contained).
    pub unix_ms: i64,
    /// Seconds since the pipeline started (matches the live log's `at_s`).
    pub at_s: f64,
    pub duration_ms: f32,
    /// Stage1 batch final (authoritative).
    pub raw_text: String,
    /// Stage1 streaming final (hotword-biased).
    pub streaming_text: String,
    /// Stage2 calibrated text.
    pub calibrated: String,
    pub intent: String,
    pub reply: String,
    pub route_ms: f64,
    /// The utterance's WAV path in the audio archive.
    pub wav: String,
}

/// Append-only, date-named JSONL turn log: `<dir>/<YYYY-MM-DD>.jsonl`.
/// Open-per-append (utterances arrive every few seconds — crash-safe beats a held handle).
pub struct TurnLog {
    dir: PathBuf,
}

impl TurnLog {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        TurnLog { dir: dir.into() }
    }

    /// Append one record to today's file.
    pub fn append(&self, rec: &TurnRecord) -> std::io::Result<()> {
        self.append_to_day(&Local::now().format("%Y-%m-%d").to_string(), rec)
    }

    /// Append to an explicit day file (the testable core; `append` supplies today).
    fn append_to_day(&self, day: &str, rec: &TurnRecord) -> std::io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&self.dir)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(format!("{day}.jsonl")))?;
        let line = serde_json::to_string(rec).expect("TurnRecord serializes");
        writeln!(f, "{line}")
    }
}

/// The Storage supervisor. Share via `Arc` between the pipeline callback and socket handlers.
pub struct Storage {
    /// Audio tier — public so handlers can serve `wav(seq)` / `list()` directly.
    pub audio: Arc<AudioArchive>,
    turns: TurnLog,
    /// Most recent turns, newest last (bounded; backs `GET /results`).
    recent: Mutex<VecDeque<TurnRecord>>,
    recent_cap: usize,
}

/// What `record_final` needs from a finalized turn (everything but the wav path, which the
/// audio archive assigns).
pub struct FinalTurn {
    pub seq: u64,
    pub at_s: f64,
    pub duration_ms: f32,
    pub raw_text: String,
    pub streaming_text: String,
    pub calibrated: String,
    pub intent: String,
    pub reply: String,
    pub route_ms: f64,
    pub pcm: Vec<i16>,
}

impl Storage {
    /// `audio`: the (already configured) audio archive. `turns_dir`: day-file directory.
    pub fn new(audio: Arc<AudioArchive>, turns_dir: impl Into<PathBuf>) -> Self {
        Storage {
            audio,
            turns: TurnLog::new(turns_dir),
            recent: Mutex::new(VecDeque::new()),
            recent_cap: 100,
        }
    }

    /// Record one finalized utterance everywhere it belongs: PCM → audio archive,
    /// transcript+decision → day log + the recent ring. Returns the built record.
    pub fn record_final(&self, t: FinalTurn) -> TurnRecord {
        let wav = self.audio.push(t.seq, t.at_s, t.pcm);
        let rec = TurnRecord {
            seq: t.seq,
            unix_ms: Local::now().timestamp_millis(),
            at_s: t.at_s,
            duration_ms: t.duration_ms,
            raw_text: t.raw_text,
            streaming_text: t.streaming_text,
            calibrated: t.calibrated,
            intent: t.intent,
            reply: t.reply,
            route_ms: t.route_ms,
            wav: wav.display().to_string(),
        };
        if let Err(e) = self.turns.append(&rec) {
            // Day-log failure must not break the live loop — the ring still serves /results.
            warn!(error = %e, seq = rec.seq, "turn log append failed");
        }
        let mut ring = self.recent.lock().unwrap();
        if ring.len() >= self.recent_cap {
            ring.pop_front();
        }
        ring.push_back(rec.clone());
        rec
    }

    /// Recent turns, oldest → newest (bounded by the ring capacity).
    pub fn recent(&self) -> Vec<TurnRecord> {
        self.recent.lock().unwrap().iter().cloned().collect()
    }

    /// Clip listing, delegated to the audio tier.
    pub fn recordings(&self) -> Vec<ClipMeta> {
        self.audio.list()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchiveConfig;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("aura-storage-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn storage(root: &std::path::Path) -> Storage {
        let audio = Arc::new(AudioArchive::new(ArchiveConfig {
            dir: root.join("recordings"),
            ..Default::default()
        }));
        Storage::new(audio, root.join("turns"))
    }

    fn turn(seq: u64) -> FinalTurn {
        FinalTurn {
            seq,
            at_s: seq as f64,
            duration_ms: 100.0,
            raw_text: format!("原文{seq}"),
            streaming_text: format!("流式{seq}"),
            calibrated: format!("整流{seq}"),
            intent: "chat".into(),
            reply: "嗯".into(),
            route_ms: 42.0,
            pcm: vec![seq as i16; 1600],
        }
    }

    #[test]
    fn record_final_feeds_all_three_sinks() {
        let root = tmp("sinks");
        let s = storage(&root);
        let rec = s.record_final(turn(1));
        // 1) audio archive: replayable, wav path recorded.
        assert!(s.audio.wav(1).is_some());
        assert!(rec.wav.contains("recordings"));
        // 2) day log: one JSONL line in today's date-named file.
        let day = Local::now().format("%Y-%m-%d").to_string();
        let log = std::fs::read_to_string(root.join("turns").join(format!("{day}.jsonl")))
            .expect("day log written");
        let parsed: TurnRecord = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.seq, 1);
        assert_eq!(parsed.calibrated, "整流1");
        assert_eq!(parsed.wav, rec.wav, "transcript links to the audio file");
        // 3) recent ring.
        assert_eq!(s.recent().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn day_log_appends_lines_in_order() {
        let root = tmp("append");
        let log = TurnLog::new(root.join("turns"));
        let mut rec = mk_rec(1);
        log.append_to_day("2026-07-17", &rec).unwrap();
        rec.seq = 2;
        log.append_to_day("2026-07-17", &rec).unwrap();
        let s = std::fs::read_to_string(root.join("turns/2026-07-17.jsonl")).unwrap();
        let seqs: Vec<u64> = s
            .lines()
            .map(|l| serde_json::from_str::<TurnRecord>(l).unwrap().seq)
            .collect();
        assert_eq!(seqs, vec![1, 2]);
        // A different day → a different file (date-named rollover).
        log.append_to_day("2026-07-18", &rec).unwrap();
        assert!(root.join("turns/2026-07-18.jsonl").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recent_ring_is_bounded() {
        let root = tmp("ring");
        let mut s = storage(&root);
        s.recent_cap = 3;
        for i in 1..=5 {
            s.record_final(turn(i));
        }
        let seqs: Vec<u64> = s.recent().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5], "oldest evicted, newest last");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn mk_rec(seq: u64) -> TurnRecord {
        TurnRecord {
            seq,
            unix_ms: 0,
            at_s: 0.0,
            duration_ms: 0.0,
            raw_text: String::new(),
            streaming_text: String::new(),
            calibrated: String::new(),
            intent: "chat".into(),
            reply: String::new(),
            route_ms: 0.0,
            wav: String::new(),
        }
    }
}
