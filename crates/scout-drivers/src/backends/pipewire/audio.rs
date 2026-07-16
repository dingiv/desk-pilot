//! Real PipeWire audio (microphone) backend.
//!
//! Unlike screen capture (which goes through the xdg-desktop-portal ScreenCast
//! session), audio connects **directly** to the local PipeWire daemon and opens
//! an input [`Stream`] on the default source. We negotiate 16 kHz mono S16LE —
//! PipeWire inserts its `audioconvert` adapter, which resamples the mic
//! (commonly 48 kHz stereo) down and remixes to mono.
//!
//! The `process` callback copies each audio buffer **once** into an `Arc<[u8]>`
//! and fans it out to every subscriber (one bounded mpsc per client). A
//! chronically slow client has individual chunks skipped (`try_send` on a full
//! channel) rather than blocking the capture thread or stalling faster clients.
//! The fan-out lock is held only for the non-blocking `try_send` sweep, so the
//! brief mutex on the RT `process` thread cannot block on I/O.
//!
//! # Verified in-container
//! Needs a running PipeWire daemon + the `audioconvert` module (`apt install
//! pipewire libpipewire-0.3-modules`). See `examples/audio_one.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use pipewire::context::Context;
use pipewire::properties::Properties;
use pipewire::spa::param::audio::{AudioFormat as SpaAudioFormat, AudioInfoRaw};
use pipewire::spa::param::format::{MediaSubtype, MediaType};
use pipewire::spa::param::format_utils;
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Pod, Value};
use pipewire::spa::utils::{Direction, SpaTypes};
use pipewire::stream::{Stream, StreamFlags};
use pipewire::thread_loop::ThreadLoop;

use crate::audio::{AudioFormat, AudioSource, AudioSubscription};
use crate::{DriverError, Result};

/// Requested sample rate (Hz). 16 kHz is the ASR wire standard; PipeWire's
/// adapter resamples the mic down to this.
const TARGET_RATE: u32 = 16000;
/// Requested channel count. Mono — speech recognition doesn't need stereo, and
/// mono halves the data rate (32 KB/s instead of 64).
const TARGET_CHANNELS: u32 = 1;
/// Per-subscriber chunk backlog (bounded mpsc capacity). With PipeWire's default
/// ~10–20 ms audio buffers this is ~0.6–1.3 s of slack before a slow client
/// starts dropping chunks.
const SUBSCRIBER_BUF: usize = 64;

/// Pause/resume command sent to the PipeWire worker thread.
#[derive(Debug, Clone, Copy)]
enum Cmd {
    Active(bool),
}

/// Shared fan-out state, written by the `process` callback + mutated by subscribe/unsubscribe.
struct Shared {
    /// One bounded sender per listening client, keyed by subscriber id.
    subscribers: Vec<(u64, SyncSender<Arc<[u8]>>)>,
    /// Negotiated format, learned from the first `Format` `param_changed`.
    fmt: Option<AudioFormat>,
    /// Any fatal worker error (connection failure, …).
    error: Option<String>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            subscribers: Vec::new(),
            fmt: None,
            error: None,
        }
    }
}

/// Monotonic subscriber ids so a dropped [`AudioSubscription`] can find + remove
/// its own sender from the fan-out.
static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// A live subscription to the audio fan-out is the shared
/// [`crate::audio::AudioSubscription`]; this backend builds one in
/// [`PipeWireAudioSource::subscribe`] with a closure that removes its sender.

/// A PipeWire-backed [`AudioSource`] (microphone capture).
pub struct PipeWireAudioSource {
    shared: Arc<Mutex<Shared>>,
    /// Pause/resume channel to the PipeWire worker (`None` if connect failed).
    ctl: Option<mpsc::Sender<Cmd>>,
    /// Keeps the PipeWire `ThreadLoop` (and its stream) alive; the worker serves
    /// [`Cmd`]s until this sender is dropped.
    worker: Option<JoinHandle<()>>,
}

impl PipeWireAudioSource {
    /// Connect to the local PipeWire daemon and start capturing the default
    /// microphone at 16 kHz mono S16LE. The stream starts *active*; call
    /// [`AudioSource::set_active`]`(false)` to pause.
    pub fn new() -> Result<Self> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (worker, ctl) = spawn(Arc::clone(&shared));
        if worker.is_none() {
            // Connect failed synchronously; surface the recorded error.
            let msg = shared
                .lock()
                .ok()
                .and_then(|g| g.error.clone())
                .unwrap_or_else(|| "PipeWire audio connect failed".into());
            return Err(DriverError::Session(msg));
        }
        Ok(Self {
            shared,
            ctl: Some(ctl),
            worker,
        })
    }
}

impl AudioSource for PipeWireAudioSource {
    fn format(&self) -> Option<AudioFormat> {
        self.shared.lock().ok().and_then(|g| g.fmt)
    }

    fn set_active(&self, active: bool) {
        if let Some(tx) = &self.ctl {
            let _ = tx.send(Cmd::Active(active));
        }
    }

    /// Add a subscriber receiving every captured audio chunk. Returns a guard
    /// whose drop unsubscribes (removes the sender from the fan-out). The stream
    /// auto-resumes on demand — call [`AudioSource::set_active`]`(true)` first if
    /// it was idle-paused.
    fn subscribe(&self) -> Result<AudioSubscription> {
        let id = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel::<Arc<[u8]>>(SUBSCRIBER_BUF);
        let mut g = self
            .shared
            .lock()
            .map_err(|_| DriverError::Backend("audio shared mutex poisoned".into()))?;
        g.subscribers.push((id, tx));
        // Unsubscribe closure: remove this id's sender when the guard drops.
        let shared_weak = Arc::downgrade(&self.shared);
        let unsub = Box::new(move || {
            if let Some(shared) = shared_weak.upgrade() {
                if let Ok(mut g) = shared.lock() {
                    g.subscribers.retain(|(sid, _)| *sid != id);
                }
            }
        });
        Ok(AudioSubscription::new(rx, unsub))
    }

    /// How many clients are currently subscribed (for idle detection: pause the
    /// mic when this hits 0).
    fn subscriber_count(&self) -> usize {
        self.shared.lock().map(|g| g.subscribers.len()).unwrap_or(0)
    }
}

impl Drop for PipeWireAudioSource {
    fn drop(&mut self) {
        // Dropping the ctl sender makes the worker's recv() return Err → it exits
        // and tears down the loop + stream. Detach the handle (joining would hang).
        let _ = self.ctl.take();
        let _ = self.worker.take();
    }
}

// ── PipeWire worker ──────────────────────────────────────────────────────────

/// Owns the PipeWire `ThreadLoop` + audio stream and fans buffers out to
/// subscribers. Returns `(None, ..)` if the connection failed synchronously.
fn spawn(shared: Arc<Mutex<Shared>>) -> (Option<JoinHandle<()>>, mpsc::Sender<Cmd>) {
    let (tx, rx) = mpsc::channel::<Cmd>();
    // Clone for the thread so `shared` stays usable in the spawn-failure branch below.
    let shared_for_thread = Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("vrover-audio".into())
        .spawn(move || run_worker(&rx, &shared_for_thread));
    match worker {
        Ok(h) => (Some(h), tx),
        Err(e) => {
            fail(&shared, format!("thread spawn: {e}"));
            (None, tx)
        }
    }
}

fn run_worker(rx: &mpsc::Receiver<Cmd>, shared: &Arc<Mutex<Shared>>) {
    // SAFETY: pw_thread_loop_new; pipewire is initialized internally by the crate.
    let tl = match unsafe { ThreadLoop::new(Some("vrover-audio"), None) } {
        Ok(t) => t,
        Err(e) => return fail(shared, format!("ThreadLoop::new: {e}")),
    };
    tl.start();
    let _lock = tl.lock(); // serialize with the loop while wiring up state.

    let context = match Context::new(&tl) {
        Ok(c) => c,
        Err(e) => return fail(shared, format!("Context::new: {e}")),
    };
    // Direct connect to the local PipeWire daemon (no portal — unlike screen).
    let _core = match context.connect(None) {
        Ok(c) => c,
        Err(e) => return fail(shared, format!("context.connect: {e}")),
    };

    let mut props = Properties::new();
    props.insert("media.type", "Audio");
    props.insert("media.category", "Capture");
    // role + AUTOCONNECT lets PipeWire route to + link the default audio source.
    props.insert("media.role", "Communication");
    let stream = match Stream::new(&_core, "vrover-audio", props) {
        Ok(s) => s,
        Err(e) => return fail(shared, format!("Stream::new: {e}")),
    };

    let shared_proc = Arc::clone(shared);
    let shared_param = Arc::clone(shared);
    let _listener = match stream
        .add_local_listener::<()>()
        .param_changed(move |_, _, id, param| {
            // The node sends its fixated Format here. Capture the real rate /
            // channels so callers (and the HTTP headers) report the truth.
            let Some(param) = param else {
                return;
            };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let mut info = AudioInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            if let Ok(mut g) = shared_param.lock() {
                g.fmt = Some(AudioFormat {
                    rate: info.rate(),
                    channels: info.channels() as u8,
                });
            }
        })
        .process(move |s, _| {
            on_process(s, &shared_proc);
        })
        .register()
    {
        Ok(l) => l,
        Err(e) => return fail(shared, format!("listener register: {e}")),
    };

    // EnumFormat: 16 kHz mono S16LE (fixed values — PipeWire's adapter resamples
    // + remixes to match). `AudioInfoRaw -> Vec<Property>` builds the property
    // list for us (no hand-rolled POD macros).
    let mut info = AudioInfoRaw::new();
    info.set_format(SpaAudioFormat::S16LE);
    info.set_rate(TARGET_RATE);
    info.set_channels(TARGET_CHANNELS);
    let obj = pipewire::spa::pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = match PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    ) {
        Ok((cursor, _len)) => cursor.into_inner(),
        Err(e) => return fail(shared, format!("audio format pod serialize: {e}")),
    };
    let Some(format_pod) = Pod::from_bytes(&values) else {
        return fail(shared, "audio format pod from_bytes failed");
    };
    let mut params = [format_pod];
    if let Err(e) = stream.connect(
        Direction::Input,
        // None + AUTOCONNECT: PipeWire links the default audio source (mic).
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    ) {
        return fail(shared, format!("audio stream.connect: {e}"));
    }

    drop(_lock);
    // Serve pause/resume until the source is dropped (sender drops → recv Err → exit).
    while let Ok(Cmd::Active(active)) = rx.recv() {
        let _g = tl.lock();
        let _ = stream.set_active(active);
    }
}

/// Copy the dequeued audio buffer once into an `Arc<[u8]>` and fan it out to
/// every subscriber. Runs on PipeWire's real-time thread (RT_PROCESS), so the
/// work here is strictly bounded: one copy + a non-blocking `try_send` sweep.
fn on_process(stream: &pipewire::stream::StreamRef, shared: &Arc<Mutex<Shared>>) {
    let Some(mut buf) = stream.dequeue_buffer() else {
        return;
    };
    let Some(data) = buf.datas_mut().get_mut(0) else {
        return;
    };
    // `chunk().size()` is the *valid* byte count (the mmap region may be larger).
    let n = data.chunk().size() as usize;
    let Some(bytes) = data.data() else {
        return;
    };
    if n == 0 || bytes.len() < n {
        return;
    }

    // One copy of the chunk, shared (cheap Arc clone) across all subscribers.
    let chunk: Arc<[u8]> = Arc::from(&bytes[..n]);

    let Ok(mut g) = shared.lock() else {
        return;
    };
    // Fan out. try_send is non-blocking: a full (slow) client skips this chunk;
    // a client whose receiver was dropped is pruned (swap_remove, O(1)).
    let mut i = 0;
    while i < g.subscribers.len() {
        match g.subscribers[i].1.try_send(Arc::clone(&chunk)) {
            Ok(()) => i += 1,
            Err(TrySendError::Full(_)) => i += 1, // slow client — keep it, skip chunk
            Err(TrySendError::Disconnected(_)) => {
                g.subscribers.swap_remove(i);
            }
        }
    }
}

fn fail(shared: &Arc<Mutex<Shared>>, msg: impl Into<String>) {
    if let Ok(mut g) = shared.lock() {
        g.error = Some(msg.into());
    }
}
