//! AsrBuffer — thread-safe **voice session** state shared between the background aura data-plane
//! client (producer) and the IME engine / frontend (consumer).
//!
//! Holds the current streaming text (`live`, updated by StreamFragment / SegmentCalibration) +
//! a stack of settled utterances (`finals`, most-recent-first, appended by WindowCalibration).
//! A `version` counter lets a poll-loop frontend (the TUI) detect changes and refresh the
//! candidate view without a keypress.
//!
//! ## Thread safety
//! The aura client writes from a background tokio thread; the IME key-event / TUI render path
//! reads from the main thread. A `std::sync::Mutex` serializes both — the lock is held only long
//! enough to clone strings (microseconds).
//!
//! ## Usage
//! ```ignore
//! let buf = AsrBuffer::new();
//! // Producer (aura data-plane thread):
//! buf.set_live("今天天气");        // StreamFragment / SegmentCalibration
//! buf.push_final("今天天气不错");   // WindowCalibration → becomes candidate #1
//! // Consumer (engine / TUI):
//! let (finals, live) = buf.voice_candidates(); // (["今天天气不错"], "今天天气")
//! let v = buf.version();                        // bumps on every write
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// Max retained settled utterances (candidate slots). Oldest evicted past this. Tunable — bump
/// for a longer recall window, but the candidate UI only shows `CANDIDATE_SLOTS` (16) anyway.
const MAX_FINALS: usize = 8;

#[derive(Default)]
struct VoiceState {
    /// Current streaming text (StreamFragment raw / SegmentCalibration). Updated continuously;
    /// cleared implicitly when a WindowCalibration graduates it (the next window overwrites it).
    live: String,
    /// Settled utterances, most-recent-first. Each WindowCalibration `push_final` inserts at the
    /// head — so `finals[0]` is the latest, which the engine surfaces as candidate #1.
    finals: Vec<String>,
}

/// Shared voice-session state. Written by the aura data-plane client, read by the IME engine.
pub struct AsrBuffer {
    state: Mutex<VoiceState>,
    /// Monotonic change counter — incremented on every write. A frontend compares it to detect
    /// "voice data changed since I last rendered" without holding the lock.
    version: AtomicU64,
    /// Aura connectivity, pushed by the data-plane client (its `/health` probe).
    /// Default false — `#asr` shows "语音不可用" until the first Connected report.
    connected: AtomicBool,
}

impl AsrBuffer {
    pub fn new() -> Self {
        AsrBuffer {
            state: Mutex::new(VoiceState::default()),
            version: AtomicU64::new(0),
            connected: AtomicBool::new(false),
        }
    }

    /// Report aura connectivity (the data-plane client's health probe).
    /// `#asr` surfaces this as "语音不可用" until Connected.
    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    /// Is the aura stream known-connected? `false` also covers "unknown yet".
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Update the live streaming text (StreamFragment / SegmentCalibration from aura).
    pub fn set_live(&self, text: &str) {
        let mut g = self.state.lock().unwrap();
        g.live = text.to_string();
        drop(g);
        self.bump();
    }

    /// A settled utterance (Final). Inserted at the head → becomes candidate #1. The finals stack
    /// is capped at [`MAX_FINALS`] (oldest evicted from the tail) so a long voice session doesn't
    /// grow the candidate list (or memory) without bound.
    pub fn push_final(&self, text: &str) {
        let mut g = self.state.lock().unwrap();
        g.finals.insert(0, text.to_string());
        if g.finals.len() > MAX_FINALS {
            g.finals.truncate(MAX_FINALS); // drop the oldest (tail)
        }
        g.live.clear(); // this utterance graduated; live awaits the next one
        drop(g);
        self.bump();
    }

    /// Snapshot for the engine: `(finals (most-recent-first), live)`. The engine builds the
    /// candidate list as `[finals..., live]` — #1 is the latest final (or the live preview if no
    /// final has arrived yet).
    pub fn voice_candidates(&self) -> (Vec<String>, String) {
        let g = self.state.lock().unwrap();
        (g.finals.clone(), g.live.clone())
    }

    /// Monotonic change counter — frontends poll this to decide whether to re-render.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Latest available text (most recent final, else the live preview, else empty). Backwards-
    /// compatible with the old single-string buffer; used by `__ASR_BUFFER__` expansion.
    pub fn snapshot(&self) -> String {
        let g = self.state.lock().unwrap();
        g.finals.first().cloned().filter(|s| !s.is_empty()).unwrap_or_else(|| g.live.clone())
    }

    /// Seed a final (used by the mock frontend's `--asr-text`). Equivalent to `push_final`.
    pub fn update(&self, text: &str) {
        self.push_final(text);
    }
}

impl Default for AsrBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let b = AsrBuffer::new();
        let (f, live) = b.voice_candidates();
        assert!(f.is_empty() && live.is_empty());
        assert_eq!(b.snapshot(), "");
    }

    #[test]
    fn connectivity_defaults_false_and_updates() {
        let b = AsrBuffer::new();
        assert!(!b.is_connected(), "unknown connectivity = unavailable");
        b.set_connected(true);
        assert!(b.is_connected());
        b.set_connected(false);
        assert!(!b.is_connected());
        // Connectivity is not a data write — it must not bump the version counter.
        let v0 = b.version();
        b.set_connected(true);
        assert_eq!(b.version(), v0, "connectivity changes don't bump version");
    }

    #[test]
    fn set_live_updates_live_and_version() {
        let b = AsrBuffer::new();
        let v0 = b.version();
        b.set_live("你好");
        let (f, live) = b.voice_candidates();
        assert_eq!(live, "你好");
        assert!(f.is_empty());
        assert!(b.version() > v0, "version must bump on set_live");
        assert_eq!(b.snapshot(), "你好", "snapshot falls back to live when no final");
    }

    #[test]
    fn push_final_inserts_at_head_and_clears_live() {
        let b = AsrBuffer::new();
        b.set_live("你好");
        b.push_final("你好世界");
        let (f, live) = b.voice_candidates();
        assert_eq!(f, vec!["你好世界"]);
        assert_eq!(live, "", "live cleared after final graduates");
        b.set_live("第二句");
        b.push_final("第二句完成");
        let (f, _) = b.voice_candidates();
        assert_eq!(f, vec!["第二句完成", "你好世界"], "most recent final first");
    }

    #[test]
    fn snapshot_prefers_latest_final() {
        let b = AsrBuffer::new();
        b.set_live("流式中");
        b.push_final("定稿一");
        b.set_live("流式二");
        assert_eq!(b.snapshot(), "定稿一", "snapshot = latest final, not live");
    }

    #[test]
    fn version_bumps_on_each_write() {
        let b = AsrBuffer::new();
        let v0 = b.version();
        b.set_live("a");
        let v1 = b.version();
        b.push_final("b");
        let v2 = b.version();
        assert!(v1 > v0 && v2 > v1);
    }

    #[test]
    fn update_seeds_a_final() {
        let b = AsrBuffer::new();
        b.update("种子文本");
        let (f, _) = b.voice_candidates();
        assert_eq!(f, vec!["种子文本"]);
    }

    #[test]
    fn finals_capped_oldest_evicted_newest_first() {
        let b = AsrBuffer::new();
        // push MAX+3 finals; the oldest (first-pushed) drop off; order stays newest-first.
        for i in 0..(MAX_FINALS + 3) as u8 {
            b.push_final(&format!("句{i}"));
        }
        let (f, _) = b.voice_candidates();
        assert_eq!(f.len(), MAX_FINALS, "capped at MAX_FINALS");
        // newest pushed = "句{MAX+2}" must be #1
        assert_eq!(f[0], format!("句{}", MAX_FINALS + 2), "newest is #1: {f:?}");
        // oldest retained = "句3" (0,1,2 evicted)
        assert_eq!(f[f.len() - 1], "句3", "oldest retained is the (MAX+3 - MAX)=3rd pushed");
    }
}
