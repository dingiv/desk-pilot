//! `media` backend — file-backed **mock** capture + audio (no hardware, no portal).
//!
//! Spawns `ffmpeg` to decode a media file into raw BGRA frames (video) and
//! 16 kHz mono S16LE PCM (audio), served through the same [`CaptureSource`] /
//! [`AudioSource`] traits as the real PipeWire backends. This lets the daemon /
//! agent run end-to-end on canned A/V with **no screen-share prompt and no mic**.
//!
//! The only runtime dependency is the `ffmpeg` (+ `ffprobe`) binary on `PATH`;
//! there is no native Rust linking, so this module is compiled unconditionally
//! (no cargo feature). If `ffmpeg` is absent or the file has no usable track,
//! the source reports a fatal error rather than spinning.
//!
//! # Pacing + looping
//! Both pipes use `-stream_loop -1` (seamless infinite loop) and `-re` so ffmpeg
//! itself paces output to the file's native framerate / the audio realtime rate.
//! The worker threads drain continuously while active and **stop when set
//! inactive** — ffmpeg then backpressures (its pipe fills, it blocks) so an idle
//! mock costs ~zero CPU, mirroring the real source's idle-pause.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{AudioFormat, AudioSource, AudioSubscription};
use crate::capture::CaptureSource;
use crate::error::{DriverError, Result};
use crate::frame::Frame;

/// Bytes read per audio fan-out tick (~128 ms of 16 kHz mono S16LE).
const AUDIO_CHUNK: usize = 4096;
/// Per-subscriber chunk backlog (bounded mpsc capacity).
const SUBSCRIBER_BUF: usize = 64;
/// Park cadence while inactive (between checking `active`/`stop`).
const POLL_INACTIVE: Duration = Duration::from_millis(50);

static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// `(width, height, fps)` probed from the file's first video stream.
fn probe_video(path: &str) -> Result<(u32, u32, u32)> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate",
            "-of",
            "csv=p=0",
            path,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| DriverError::Session(format!("ffprobe spawn: {e}")))?;
    if !out.status.success() {
        return Err(DriverError::Session(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = line.trim().split(',').collect();
    if parts.len() < 3 {
        return Err(DriverError::Session(format!(
            "ffprobe: no video stream in {path} ({line})"
        )));
    }
    let w: u32 = parts[0]
        .parse()
        .map_err(|_| DriverError::Session(format!("bad width {}", parts[0])))?;
    let h: u32 = parts[1]
        .parse()
        .map_err(|_| DriverError::Session(format!("bad height {}", parts[1])))?;
    // r_frame_rate is "num/den" (e.g. "60/1"); fps = num/den.
    let fps = (|| {
        let (n, d) = parts[2].split_once('/')?;
        let n: f64 = n.parse().ok()?;
        let d: f64 = d.parse().ok()?;
        (d > 0.0).then_some((n / d).round() as u32)
    })()
    .unwrap_or(30);
    Ok((w, h, fps.max(1)))
}

/// Spawn the video-decoding ffmpeg child → raw BGRA frames on stdout.
fn spawn_video_ffmpeg(path: &str) -> std::io::Result<Child> {
    Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-nostdin",
            "-re",
            "-stream_loop",
            "-1",
            "-i",
            path,
            "-an",
            "-map",
            "0:v:0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// Spawn the audio-decoding ffmpeg child → 16 kHz mono S16LE on stdout.
fn spawn_audio_ffmpeg(path: &str) -> std::io::Result<Child> {
    Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-nostdin",
            "-re",
            "-stream_loop",
            "-1",
            "-i",
            path,
            "-vn",
            "-map",
            "0:a:0",
            "-f",
            "s16le",
            "-ar",
            "16000",
            "-ac",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

// ── video ────────────────────────────────────────────────────────────────────

struct FrameSlot {
    buf: Vec<u8>,
    ready: bool,
    error: Option<String>,
}

/// File-backed mock [`CaptureSource`]: decodes the file's video track to BGRA at
/// native resolution/framerate, looping forever, paused when set inactive.
pub struct MediaVideoSource {
    width: u32,
    height: u32,
    slot: Arc<Mutex<FrameSlot>>,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MediaVideoSource {
    /// Probe `path` and start the realtime-looping video feeder. Errors if
    /// `ffprobe`/`ffmpeg` are missing or the file has no video stream.
    pub fn new(path: &str) -> Result<Self> {
        let (w, h, _fps) = probe_video(path)?;
        let need = (w as usize) * (h as usize) * 4;
        let slot = Arc::new(Mutex::new(FrameSlot {
            buf: vec![0u8; need],
            ready: false,
            error: None,
        }));
        let active = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let path = path.to_string();
            let slot = Arc::clone(&slot);
            let active = Arc::clone(&active);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("vrover-media-video".into())
                .spawn(move || video_worker(&path, w, h, &slot, &active, &stop))
                .map_err(|e| DriverError::Session(format!("video worker spawn: {e}")))?
        };
        Ok(Self {
            width: w,
            height: h,
            slot,
            active,
            stop,
            worker: Some(worker),
        })
    }

    /// The probed geometry.
    #[must_use]
    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl CaptureSource for MediaVideoSource {
    fn size(&self) -> Option<(u32, u32)> {
        Some((self.width, self.height))
    }

    fn capture(&mut self) -> Result<Frame> {
        let g = self
            .slot
            .lock()
            .map_err(|_| DriverError::Backend("video slot mutex poisoned".into()))?;
        if let Some(ref e) = g.error {
            return Err(DriverError::Session(e.clone()));
        }
        if !g.ready {
            return Err(DriverError::Session(
                "no video frame yet (waiting for first decode)".into(),
            ));
        }
        Frame::new(self.width, self.height, g.buf.clone())
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    fn clear_frame(&self) {
        if let Ok(mut g) = self.slot.lock() {
            g.ready = false;
        }
    }
}

impl Drop for MediaVideoSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.active.store(true, Ordering::Relaxed); // unpark the worker so it sees `stop`
        let _ = self.worker.take(); // detach (worker exits within ~one frame)
    }
}

/// Continuously decode frames into `slot` while `active`; respawn on EOF; exit
/// when `stop`. ffmpeg's `-re` paces to native framerate.
fn video_worker(
    path: &str,
    w: u32,
    h: u32,
    slot: &Arc<Mutex<FrameSlot>>,
    active: &AtomicBool,
    stop: &AtomicBool,
) {
    let need = (w as usize) * (h as usize) * 4;
    while !stop.load(Ordering::Relaxed) {
        if !active.load(Ordering::Relaxed) {
            thread::sleep(POLL_INACTIVE);
            continue;
        }
        let mut child = match spawn_video_ffmpeg(path) {
            Ok(c) => c,
            Err(e) => return fail(slot, format!("ffmpeg spawn: {e}")),
        };
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut got_any = false;
        while !stop.load(Ordering::Relaxed) && active.load(Ordering::Relaxed) {
            let mut buf = vec![0u8; need];
            match stdout.read_exact(&mut buf) {
                Ok(()) => {
                    got_any = true;
                    if let Ok(mut g) = slot.lock() {
                        g.buf.copy_from_slice(&buf);
                        g.ready = true;
                    }
                }
                Err(_) => break, // EOF or ffmpeg died → respawn (or fatal below)
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        // ffmpeg exited before producing a single frame → fatal (no/bad video),
        // not a transient EOF: don't busy-loop the respawn.
        if !got_any && !stop.load(Ordering::Relaxed) {
            return fail(slot, format!("ffmpeg produced no video frames from {path}"));
        }
    }
}

// ── audio ────────────────────────────────────────────────────────────────────

/// File-backed mock [`AudioSource`]: decodes the ENTIRE file to 16 kHz mono S16LE
/// PCM **once** at construction (one-shot ffmpeg, no realtime pacing, no loop).
/// Each subscriber gets its own **independent replay from the beginning**, paced
/// to realtime, one-shot (stops at the end — no loop). This makes tests
/// reproducible: every client hears the same audio from the same starting point.
pub struct MediaAudioSource {
    pcm: Arc<Vec<u8>>,
    active_count: Arc<AtomicUsize>,
}

impl MediaAudioSource {
    pub fn new(path: &str) -> Result<Self> {
        let mut child = spawn_audio_ffmpeg_oneshot(path)
            .map_err(|e| DriverError::Session(format!("ffmpeg audio spawn: {e}")))?;
        let mut pcm = Vec::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .read_to_end(&mut pcm)
            .map_err(|e| DriverError::Session(format!("ffmpeg audio read: {e}")))?;
        let _ = child.wait();
        if pcm.is_empty() {
            return Err(DriverError::Session(format!(
                "ffmpeg produced no audio from {path}"
            )));
        }
        let dur = pcm.len() as f64 / (16000.0 * 2.0);
        tracing::info!(path = %path, bytes = pcm.len(), dur_s = dur, "media audio decoded (16k mono)");
        Ok(Self {
            pcm: Arc::new(pcm),
            active_count: Arc::new(AtomicUsize::new(0)),
        })
    }
}

impl AudioSource for MediaAudioSource {
    fn format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { rate: 16_000, channels: 1 })
    }

    /// No-op: per-subscriber threads are independently paced. The idle ticker
    /// checks `subscriber_count` to decide pausing — that's enough.
    fn set_active(&self, _active: bool) {}

    fn subscribe(&self) -> Result<AudioSubscription> {
        let (tx, rx) = mpsc::sync_channel::<Arc<[u8]>>(SUBSCRIBER_BUF);
        let pcm = Arc::clone(&self.pcm);
        let active_count = Arc::clone(&self.active_count);
        active_count.fetch_add(1, Ordering::Relaxed);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);

        thread::Builder::new()
            .name("vrover-media-audio-replay".into())
            .spawn(move || replay_from_start(&pcm, tx, &stop_for_thread, &active_count))
            .map_err(|e| DriverError::Session(format!("replay thread spawn: {e}")))?;

        let stop_for_unsub = Arc::clone(&stop);
        let active_count_unsub = Arc::clone(&self.active_count);
        let unsub = Box::new(move || {
            stop_for_unsub.store(true, Ordering::Relaxed);
            // The replay thread will also dec-count on exit; but do it here too in case
            // the thread is sleeping.
            active_count_unsub.fetch_sub(1, Ordering::Relaxed);
        });
        Ok(AudioSubscription::new(rx, unsub))
    }

    fn subscriber_count(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }
}

/// Replay the in-memory PCM from the beginning at realtime pace, one-shot. Sends
/// 20ms chunks via the bounded channel; stops at EOF or when `stop` is set.
fn replay_from_start(
    pcm: &[u8],
    tx: SyncSender<Arc<[u8]>>,
    stop: &AtomicBool,
    active_count: &AtomicUsize,
) {
    let bytes_per_sec: usize = 16000 * 2; // 16kHz mono S16LE
    let chunk_bytes = bytes_per_sec * 20 / 1000; // 20ms = 640 bytes
    let start = Instant::now();
    let mut offset = 0;

    while !stop.load(Ordering::Relaxed) && offset < pcm.len() {
        let end = (offset + chunk_bytes).min(pcm.len());
        let chunk: Arc<[u8]> = Arc::from(&pcm[offset..end]);
        if tx.send(chunk).is_err() {
            break; // client disconnected
        }
        offset = end;

        // Pace to realtime: if audio consumed > wall time elapsed, sleep the diff.
        let audio_ms = (offset as u64 * 1000) / bytes_per_sec as u64;
        let wall_ms = start.elapsed().as_millis() as u64;
        if audio_ms > wall_ms {
            thread::sleep(Duration::from_millis(audio_ms - wall_ms));
        }
    }
    active_count.fetch_sub(1, Ordering::Relaxed);
}

// ── mock-audio directory source ──────────────────────────────────────────────

/// Directory-backed mock [`AudioSource`]: holds a directory of audio files.
/// On each `subscribe()`, a random file is picked, decoded on-demand (no
/// preheating), and replayed at realtime pace. Each subscriber gets an
/// independent, randomly-chosen file.
pub struct MockAudioDirSource {
    files: Vec<String>,
}

impl MockAudioDirSource {
    /// Scan `dir` for audio files (m4a, wav, mp3, flac, ogg). Errors if none found.
    pub fn new(dir: &str) -> Result<Self> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            DriverError::Session(format!("mock-audio dir {dir}: {e}"))
        })?;
        let exts = ["m4a", "wav", "mp3", "flac", "ogg", "webm", "opus"];
        let mut files: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let ext = p.extension()?.to_str()?.to_lowercase();
                exts.contains(&ext.as_str()).then(|| p.to_string_lossy().into_owned())
            })
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(DriverError::Session(format!(
                "mock-audio dir {dir} has no audio files (supported: {:?})",
                exts
            )));
        }
        tracing::info!(dir = %dir, count = files.len(), "mock-audio directory ready");
        Ok(Self { files })
    }
}

impl AudioSource for MockAudioDirSource {
    fn format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { rate: 16_000, channels: 1 })
    }

    fn set_active(&self, _active: bool) {}

    fn subscribe(&self) -> Result<AudioSubscription> {
        // Pick a random file.
        let idx = (NEXT_SUB.fetch_add(1, Ordering::Relaxed) as usize) % self.files.len();
        let path = self.files[idx].clone();
        tracing::info!(path = %path, idx, "mock-audio: decoding for new subscriber");

        // Decode on-demand (no preheating): one-shot ffmpeg → 16kHz mono S16LE in memory.
        let mut child = spawn_audio_ffmpeg_oneshot(&path)
            .map_err(|e| DriverError::Session(format!("ffmpeg spawn for {path}: {e}")))?;
        let mut pcm = Vec::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .read_to_end(&mut pcm)
            .map_err(|e| DriverError::Session(format!("ffmpeg read {path}: {e}")))?;
        let _ = child.wait();
        if pcm.is_empty() {
            return Err(DriverError::Session(format!("ffmpeg produced no audio from {path}")));
        }
        let pcm: Arc<Vec<u8>> = Arc::new(pcm);
        let dur = pcm.len() as f64 / (16000.0 * 2.0);
        tracing::info!(path = %path, dur_s = dur, "mock-audio: decoded, starting replay");

        // Same replay mechanism as MediaAudioSource.
        let (tx, rx) = mpsc::sync_channel::<Arc<[u8]>>(SUBSCRIBER_BUF);
        let active_count = Arc::new(AtomicUsize::new(0)); // not shared across subscribers; per-sub
        active_count.fetch_add(1, Ordering::Relaxed);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let ac = Arc::clone(&active_count);

        thread::Builder::new()
            .name("omni-scout-mock-replay".into())
            .spawn(move || replay_from_start(&pcm, tx, &stop_for_thread, &ac))
            .map_err(|e| DriverError::Session(format!("replay thread spawn: {e}")))?;

        let stop_for_unsub = Arc::clone(&stop);
        let unsub = Box::new(move || {
            stop_for_unsub.store(true, Ordering::Relaxed);
        });
        Ok(AudioSubscription::new(rx, unsub))
    }

    fn subscriber_count(&self) -> usize {
        0 // we don't track global count (each subscriber is independent)
    }
}

/// One-shot audio decode: no `-re` (decode as fast as possible), no `-stream_loop`
/// (no loop). Produces the entire file as 16 kHz mono S16LE on stdout.
fn spawn_audio_ffmpeg_oneshot(path: &str) -> std::io::Result<Child> {
    Command::new("ffmpeg")
        .args([
            "-loglevel", "error", "-nostdin",
            "-i", path,
            "-vn", "-map", "0:a:0",
            "-f", "s16le", "-ar", "16000", "-ac", "1",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

fn fail<T>(slot_or_shared: &Arc<Mutex<T>>, msg: String)
where
    T: ErrorSink,
{
    if let Ok(mut g) = slot_or_shared.lock() {
        g.set_error(msg);
    }
}

trait ErrorSink {
    fn set_error(&mut self, msg: String);
}
impl ErrorSink for FrameSlot {
    fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
    }
}
