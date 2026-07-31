//! archive — the audio management module: owns every recorded utterance clip.
//!
//! Two tiers:
//! - **Hot (in-memory)**: a bounded `seq → Clip` map holding the most recent utterances for
//!   instant replay (the web UI's playback button).
//! - **Cold (disk)**: WAV files named by date — `<dir>/<YYYY-MM-DD>/<HHMMSS>_<seq:04>.wav`
//!   (capture wall-time, local timezone; the day dir rolls over automatically at midnight) —
//!   written by a periodic flusher ([`AudioArchive::spawn_flusher`]) or an explicit
//!   [`AudioArchive::flush_now`].
//!
//! Invariants:
//! - An **unflushed clip is never evicted** — overflow flushes it to disk first, so audio is
//!   never silently lost.
//! - Playback ([`AudioArchive::wav`]) is transparent: hot tier first, then the flushed file
//!   (a run-lifetime `seq → path` index survives eviction).
//! - [`AudioArchive::push`] returns the clip's destined WAV path, so upper layers (the Storage
//!   hub's turn log) can reference the recording before it is even flushed.
//!
//! Filesystem writes are the archive's own contained side effect — callers (the daemon) just
//! `push` PCM and serve `wav` bytes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local};
use serde::Serialize;
use tracing::{debug, error, warn};

use crate::wav;

/// Config for [`AudioArchive`].
#[derive(Debug, Clone)]
pub struct ArchiveConfig {
    /// Recordings ROOT — clips land in per-day subdirs (`<dir>/<YYYY-MM-DD>/`), created lazily.
    pub dir: PathBuf,
    /// Max clips held in the hot tier; the oldest (flushed) clip is evicted beyond this.
    pub hot_capacity: usize,
    /// Cadence of the background flusher thread.
    pub flush_every: Duration,
    /// PCM sample rate (mono S16LE) — 16 kHz across the pipeline.
    pub sample_rate: u32,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        ArchiveConfig {
            dir: PathBuf::from("data/recordings"),
            hot_capacity: 30,
            flush_every: Duration::from_secs(10),
            sample_rate: 16_000,
        }
    }
}

/// One clip's metadata (what `GET /api/recordings` returns per entry).
#[derive(Debug, Clone, Serialize)]
pub struct ClipMeta {
    pub seq: u64,
    /// Wall-clock seconds since the pipeline started (0.0 when only known from disk).
    pub at_s: f64,
    pub duration_ms: f32,
    /// Still in the hot tier (instant replay)?
    pub hot: bool,
    /// The clip's WAV path (destined at push; the file exists once flushed).
    pub path: String,
}

struct Clip {
    pcm: Vec<i16>,
    at_s: f64,
    /// Destined WAV path, fixed at push time (date-named from the capture wall-time).
    path: PathBuf,
    flushed: bool,
}

/// Shared mutable state: bounded hot tier + a run-lifetime index of flushed clip paths.
struct Inner {
    hot: BTreeMap<u64, Clip>,
    /// Every flushed clip's path, kept after eviction (backs disk replay + listing).
    flushed: BTreeMap<u64, PathBuf>,
}

/// The audio manager: bounded hot tier + periodically-flushed date-named WAV cold tier.
/// Thread-safe — share via `Arc` between the pipeline callback (push) and socket handlers.
pub struct AudioArchive {
    cfg: ArchiveConfig,
    inner: Mutex<Inner>,
}

/// Relative (day-dir, file-name) for a clip captured at `t`:
/// `("2026-07-17", "133712_0007.wav")`.
fn clip_rel_path(t: &DateTime<Local>, seq: u64) -> (String, String) {
    (
        t.format("%Y-%m-%d").to_string(),
        format!("{}_{seq:04}.wav", t.format("%H%M%S")),
    )
}

impl AudioArchive {
    pub fn new(cfg: ArchiveConfig) -> Self {
        AudioArchive {
            cfg,
            inner: Mutex::new(Inner { hot: BTreeMap::new(), flushed: BTreeMap::new() }),
        }
    }

    /// Add a finalized utterance's PCM; returns the clip's destined date-named WAV path.
    /// Evicts the oldest hot clip beyond capacity — flushing it to disk first if it wasn't
    /// yet (audio is never dropped).
    pub fn push(&self, seq: u64, at_s: f64, pcm: Vec<i16>) -> PathBuf {
        let (day, name) = clip_rel_path(&Local::now(), seq);
        let path = self.cfg.dir.join(day).join(name);
        let mut g = self.inner.lock().unwrap();
        g.hot.insert(seq, Clip { pcm, at_s, path: path.clone(), flushed: false });
        while g.hot.len() > self.cfg.hot_capacity {
            let (&oldest, clip) = g.hot.iter().next().expect("len > cap ⇒ non-empty");
            if !clip.flushed {
                if let Err(e) = write_clip(clip, self.cfg.sample_rate) {
                    // Disk failed: keep the clip in memory rather than lose audio.
                    warn!(seq = oldest, error = %e, "overflow flush failed — keeping clip hot");
                    break;
                }
            }
            let clip = g.hot.remove(&oldest).expect("just observed");
            g.flushed.insert(oldest, clip.path);
        }
        path
    }

    /// WAV bytes for playback: hot tier first, else the flushed file on disk.
    pub fn wav(&self, seq: u64) -> Option<Vec<u8>> {
        let path = {
            let g = self.inner.lock().unwrap();
            if let Some(clip) = g.hot.get(&seq) {
                return Some(wav::wav_bytes(&clip.pcm, self.cfg.sample_rate));
            }
            g.flushed.get(&seq).cloned()?
        };
        std::fs::read(path).ok()
    }

    /// All clips of this run (hot ∪ flushed), ascending seq.
    pub fn list(&self) -> Vec<ClipMeta> {
        let g = self.inner.lock().unwrap();
        let mut out: BTreeMap<u64, ClipMeta> = BTreeMap::new();
        for (&seq, path) in &g.flushed {
            let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            out.insert(
                seq,
                ClipMeta {
                    seq,
                    at_s: 0.0,
                    duration_ms: wav_duration_ms(bytes, self.cfg.sample_rate),
                    hot: false,
                    path: path.display().to_string(),
                },
            );
        }
        // Hot tier wins on metadata (it knows at_s; a clip can be flushed AND still hot).
        for (&seq, clip) in &g.hot {
            out.insert(
                seq,
                ClipMeta {
                    seq,
                    at_s: clip.at_s,
                    duration_ms: clip.pcm.len() as f32 / self.cfg.sample_rate as f32 * 1000.0,
                    hot: true,
                    path: clip.path.display().to_string(),
                },
            );
        }
        out.into_values().collect()
    }

    /// Write every unflushed hot clip to disk. Returns how many were written.
    pub fn flush_now(&self) -> std::io::Result<usize> {
        let mut g = self.inner.lock().unwrap();
        let mut written = 0;
        let pending: Vec<u64> =
            g.hot.iter().filter(|(_, c)| !c.flushed).map(|(&s, _)| s).collect();
        for seq in pending {
            let clip = g.hot.get(&seq).expect("collected above");
            write_clip(clip, self.cfg.sample_rate)?;
            let path = clip.path.clone();
            g.hot.get_mut(&seq).expect("collected above").flushed = true;
            g.flushed.insert(seq, path);
            written += 1;
        }
        if written > 0 {
            debug!(written, dir = %self.cfg.dir.display(), "archive flushed");
        }
        Ok(written)
    }

    /// How many hot clips are not yet on disk (diagnostics / tests).
    pub fn pending(&self) -> usize {
        self.inner.lock().unwrap().hot.values().filter(|c| !c.flushed).count()
    }

    /// Spawn the periodic flusher thread. Holds only a `Weak` — the thread exits on its own
    /// once the archive is dropped, so the daemon needs no shutdown plumbing.
    pub fn spawn_flusher(self: &Arc<Self>) -> thread::JoinHandle<()> {
        let weak: Weak<Self> = Arc::downgrade(self);
        let every = self.cfg.flush_every;
        thread::Builder::new()
            .name("aura-archive".into())
            .spawn(move || loop {
                thread::sleep(every);
                let Some(archive) = weak.upgrade() else { break };
                if let Err(e) = archive.flush_now() {
                    error!(error = %e, "periodic archive flush failed");
                }
            })
            .expect("spawn aura-archive flusher")
    }
}

fn write_clip(clip: &Clip, sample_rate: u32) -> std::io::Result<()> {
    if let Some(parent) = clip.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    wav::save_wav(&clip.path, &clip.pcm, sample_rate)
}

/// Duration from a WAV file's byte size (44-byte header + 2 bytes/sample mono).
fn wav_duration_ms(file_bytes: u64, sample_rate: u32) -> f32 {
    let data = file_bytes.saturating_sub(44);
    (data as f32 / 2.0) / sample_rate as f32 * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Unique temp dir per test (no tempfile dep).
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("aura-archive-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn cfg(dir: &std::path::Path, cap: usize) -> ArchiveConfig {
        ArchiveConfig { dir: dir.to_path_buf(), hot_capacity: cap, ..Default::default() }
    }

    fn pcm(marker: i16) -> Vec<i16> {
        vec![marker; 1600] // 100ms @ 16k
    }

    #[test]
    fn date_based_naming() {
        // Format is derived from the local wall-time components, TZ-independent to assert.
        let t = Local.with_ymd_and_hms(2026, 7, 17, 13, 37, 12).unwrap();
        let (day, name) = clip_rel_path(&t, 7);
        assert_eq!(day, "2026-07-17");
        assert_eq!(name, "133712_0007.wav");
    }

    #[test]
    fn push_returns_dated_path_and_replays_hot() {
        let dir = tmp("hot");
        let a = AudioArchive::new(cfg(&dir, 5));
        let path = a.push(1, 0.5, pcm(7));
        assert!(path.starts_with(&dir), "under the recordings root");
        let day = path.parent().unwrap().file_name().unwrap().to_str().unwrap();
        assert_eq!(day.len(), 10, "YYYY-MM-DD day dir, got {day}");
        assert!(path.file_name().unwrap().to_str().unwrap().ends_with("_0001.wav"));
        let wav = a.wav(1).expect("hot replay");
        assert_eq!(&wav[..4], b"RIFF");
        assert!(a.wav(99).is_none(), "unknown seq");
    }

    #[test]
    fn flush_then_replay_from_disk_after_eviction() {
        let dir = tmp("flush");
        let a = AudioArchive::new(cfg(&dir, 2));
        let p1 = a.push(1, 0.1, pcm(1));
        a.push(2, 0.2, pcm(2));
        assert_eq!(a.flush_now().unwrap(), 2);
        assert!(p1.exists(), "flushed to the dated path");
        assert_eq!(a.pending(), 0);
        // Overflow evicts seq 1 (already flushed → plain eviction) — still replayable.
        a.push(3, 0.3, pcm(3));
        let wav1 = a.wav(1).expect("evicted clip must replay from disk");
        assert_eq!(&wav1[..4], b"RIFF");
        assert_eq!(wav1.len(), 44 + 1600 * 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overflow_flushes_unflushed_before_evicting() {
        let dir = tmp("overflow");
        let a = AudioArchive::new(cfg(&dir, 1));
        let p1 = a.push(1, 0.1, pcm(1)); // never explicitly flushed
        a.push(2, 0.2, pcm(2)); // overflow → seq 1 must be written, then evicted
        assert!(p1.exists(), "unflushed clip written before eviction");
        assert!(a.wav(1).is_some(), "still replayable from disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_merges_hot_and_flushed() {
        let dir = tmp("list");
        let a = AudioArchive::new(cfg(&dir, 1));
        a.push(1, 0.1, pcm(1));
        a.push(2, 0.2, pcm(2)); // seq 1 → flushed+evicted, seq 2 → hot
        let metas = a.list();
        assert_eq!(metas.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1, 2]);
        let m1 = &metas[0];
        assert!(!m1.hot && m1.path.ends_with("_0001.wav"));
        assert!((m1.duration_ms - 100.0).abs() < 1.0, "duration from file size");
        let m2 = &metas[1];
        assert!(m2.hot && (m2.at_s - 0.2).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
