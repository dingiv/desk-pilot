//! Real PipeWire backend (behind the `pipewire` feature).
//!
//! Flow: an [`ashpd`] `ScreenCast` portal session is negotiated on a one-shot
//! tokio runtime (→ PipeWire node id + fd + frame size), then a pipewire
//! [`ThreadLoop`] connects to that fd and a [`Stream`] consumes frames. The
//! `process` callback decodes the common BGRx/BGRA mmap'd buffer into a [`Frame`]
//! stored behind an `Arc<Mutex>`; [`PipeWireSource::capture`] hands back the latest.
//!
//! # Verified in-container (2026-06-24).
//! The dev container mounts the host GNOME/Wayland desktop, so this runs here:
//! an ashpd ScreenCast session → PipeWire node → mmap'd BGRx/BGRA frame → PNG,
//! end to end (see `examples/capture_one.rs`). It needs `libpipewire-0.3-modules`
//! (protocol-native + adapter) + the `pipewire` runtime client.conf to run, and
//! the portal prompts to pick a screen each run (`PersistMode::DoNot`).
//! Remaining gaps (search for `TODO(host)`):
//! - DMA-BUF / hardware-locked buffers (only the mmap'd pointer path is handled);
//! - multi-monitor stream selection (we take stream 0).
//! - cursor-mode / restore-token knobs exposed via [`PipeWireSourceBuilder`].

use std::os::unix::io::OwnedFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use pipewire::context::Context;
use pipewire::properties::Properties;
use pipewire::spa::param::format::{FormatProperties, MediaType, MediaSubtype};
use pipewire::spa::param::format_utils;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Pod, Value};
use pipewire::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};
use pipewire::stream::{Stream, StreamFlags};
use pipewire::thread_loop::ThreadLoop;
use crate::{CaptureSource, DriverError, Frame, Result};

/// Cap on how many frames/second we actually copy + decode, even when the
/// compositor pushes faster (e.g. 60fps). Excess buffers are dequeued (returned
/// to the producer, so no backpressure) but not decoded → caps our CPU.
const TARGET_FPS: u32 = 24;
/// Minimum interval between decoded frames: `1 / TARGET_FPS` (~41.6ms at 24fps).
const MIN_FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / TARGET_FPS as u64);

/// Pause/resume command sent to the pipewire worker thread.
#[derive(Debug, Clone, Copy)]
enum Cmd {
    Active(bool),
}

/// Builder for [`PipeWireSource`].
#[derive(Debug, Clone)]
#[must_use]
pub struct PipeWireSourceBuilder {
    cursor_mode: CursorMode,
}

impl Default for PipeWireSourceBuilder {
    fn default() -> Self {
        Self {
            cursor_mode: CursorMode::Embedded,
        }
    }
}

impl PipeWireSourceBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// How the cursor is drawn into the captured frames. Defaults to
    /// [`CursorMode::Embedded`] (cursor composited in-frame).
    pub fn cursor_mode(mut self, mode: CursorMode) -> Self {
        self.cursor_mode = mode;
        self
    }

    /// Negotiate the ScreenCast session and start streaming. Talks to
    /// xdg-desktop-portal, so it needs a real graphical session (fails with a
    /// [`DriverError::Session`] otherwise).
    pub fn build(&self) -> Result<PipeWireSource> {
        PipeWireSource::start(self.cursor_mode)
    }
}

/// Latest decoded frame + any fatal worker error.
#[derive(Default)]
struct Shared {
    /// Reused BGRA pixel buffer — resized once, copied into each decoded frame
    /// (avoids a per-frame ~39MB allocation). [`capture`] clones it on demand.
    frame_buf: Vec<u8>,
    /// Negotiated stream geometry, learned from the first `Format`
    /// `param_changed`. [`decode_frame`] decodes against this (not the
    /// portal-reported size, which may differ once the node fixates).
    dims: Option<(u32, u32)>,
    /// Has at least one frame been decoded into `frame_buf`?
    frame_ready: bool,
    /// Last decoded-frame time, for the [`TARGET_FPS`] throttle.
    last_decode: Option<Instant>,
    error: Option<String>,
}

/// A PipeWire-backed [`CaptureSource`].
pub struct PipeWireSource {
    latest: Arc<Mutex<Shared>>,
    dims: Option<(u32, u32)>,
    /// Pause/resume channel to the pipewire worker (`None` if negotiation failed).
    ctl: Option<mpsc::Sender<Cmd>>,
    /// Keeps the pipewire [`ThreadLoop`] (and its stream) alive; the worker serves
    /// [`Cmd`]s until this sender is dropped.
    worker: Option<JoinHandle<()>>,
}

impl PipeWireSource {
    /// Negotiate + start, taking the first portal stream, no restore token.
    pub fn new() -> Result<Self> {
        PipeWireSourceBuilder::new().build()
    }

    fn start(cursor_mode: CursorMode) -> Result<Self> {
        // 1. Negotiate the ScreenCast session on a throwaway tokio runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DriverError::Session(format!("tokio runtime: {e}")))?;

        let (fd, node_id, dims) = rt.block_on(async {
            let proxy = Screencast::new().await.map_err(session_err)?;
            let session = proxy.create_session().await.map_err(session_err)?;
            proxy
                .select_sources(
                    &session,
                    cursor_mode,
                    // BitFlags<SourceType> from a single variant.
                    SourceType::Monitor.into(),
                    false,
                    None,
                    PersistMode::DoNot,
                )
                .await
                .map_err(session_err)?;
            // start → Request<Streams>; .response() yields the Streams once the
            // portal has answered (the .await above already waited for it).
            let response = proxy
                .start(&session, None)
                .await
                .map_err(session_err)?
                .response()
                .map_err(session_err)?;
            let stream = response
                .streams()
                .first()
                .ok_or_else(|| DriverError::Session("portal returned no streams".into()))?;
            let node_id = stream.pipe_wire_node_id();
            let size = stream.size();
            let fd = proxy
                .open_pipe_wire_remote(&session)
                .await
                .map_err(session_err)?;
            Ok::<_, DriverError>((fd, node_id, size))
        })?;

        let dims = dims.and_then(|(w, h)| {
            (w > 0 && h > 0).then(|| (w as u32, h as u32))
        });

        // 2. Spawn the pipewire thread owning the loop + stream.
        let latest = Arc::new(Mutex::new(Shared::default()));
        let (worker, ctl) = spawn_pipewire(fd, node_id, dims, Arc::clone(&latest));

        Ok(Self {
            latest,
            dims,
            ctl: Some(ctl),
            worker: Some(worker),
        })
    }
}

impl Default for PipeWireSource {
    fn default() -> Self {
        // Portal negotiation is fallible; surface failure loudly rather than
        // pretend a default exists.
        Self::new().unwrap_or_else(|_| Self::failed())
    }
}

impl CaptureSource for PipeWireSource {
    fn size(&self) -> Option<(u32, u32)> {
        self.dims
    }

    fn capture(&mut self) -> Result<Frame> {
        let shared = self.latest.lock().expect("pipewire shared mutex poisoned");
        if let Some(ref err) = shared.error {
            return Err(DriverError::Session(err.clone()));
        }
        if !shared.frame_ready {
            return Err(DriverError::Session(
                "no frame yet (waiting for first pipewire buffer)".into(),
            ));
        }
        let (w, h) = shared
            .dims
            .ok_or_else(|| DriverError::Session("stream geometry not negotiated yet".into()))?;
        Frame::new(w, h, shared.frame_buf.clone())
    }

    /// Pause (`false`) or resume (`true`) the PipeWire stream. Pausing makes the
    /// producer (Mutter) stop pushing frames → ~zero capture cost while idle.
    /// No-op in the failed state.
    fn set_active(&self, active: bool) {
        if let Some(tx) = &self.ctl {
            let _ = tx.send(Cmd::Active(active));
        }
    }

    /// Drop the cached latest frame so callers block until a fresh one arrives
    /// (used after resuming an idle-paused stream, to avoid serving a stale frame).
    fn clear_frame(&self) {
        if let Ok(mut g) = self.latest.lock() {
            g.frame_ready = false;
        }
    }
}

impl Drop for PipeWireSource {
    fn drop(&mut self) {
        // Dropping the JoinHandle detaches the worker thread, which is parked by
        // design (joining would hang). The pipe + stream live for the worker's
        // lifetime; they're torn down when the process tears the worker down.
        let _ = self.worker.take();
    }
}

impl PipeWireSource {
    /// A non-functional source used only by [`Default`] when the portal is absent.
    fn failed() -> Self {
        let latest = Arc::new(Mutex::new(Shared {
            frame_buf: Vec::new(),
            dims: None,
            frame_ready: false,
            last_decode: None,
            error: Some("PipeWire negotiation failed (no portal / not on a graphical session)".into()),
        }));
        Self {
            latest,
            dims: None,
            ctl: None,
            worker: None,
        }
    }
}

// ── pipewire worker ──────────────────────────────────────────────────────────

/// Owns the pipewire `ThreadLoop` + stream and decodes frames into `latest`.
/// Returns a parked thread whose lifetime == the source's.
fn spawn_pipewire(
    fd: OwnedFd,
    node_id: u32,
    dims: Option<(u32, u32)>,
    latest: Arc<Mutex<Shared>>,
) -> (JoinHandle<()>, mpsc::Sender<Cmd>) {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let worker = thread::spawn(move || {
        // Portal-reported geometry — used as the default in the EnumFormat size
        // range below. The actually-negotiated size arrives via param_changed and
        // is what decode_frame uses (see Shared::dims).
        let (w, h) = match dims {
            Some(d) => d,
            None => return fail(&latest, "portal gave no stream size"),
        };

        // SAFETY: pw_thread_loop_new; the only documented requirement is that
        // pipewire is initialized, which the crate handles internally.
        let tl = match unsafe { ThreadLoop::new(Some("vrover-pipewire"), None) } {
            Ok(tl) => tl,
            Err(e) => return fail(&latest, format!("ThreadLoop::new: {e}")),
        };
        // Start the loop's background thread before touching context/state.
        tl.start();
        let _lock = tl.lock(); // serialize with the loop while we wire up state.

        let context = match Context::new(&tl) {
            Ok(c) => c,
            Err(e) => return fail(&latest, format!("Context::new: {e}")),
        };
        let core = match context.connect_fd(fd, None) {
            Ok(c) => c,
            Err(e) => return fail(&latest, format!("connect_fd: {e}")),
        };

        let mut props = Properties::new();
        props.insert("media.type", "Video");
        props.insert("media.category", "Capture");
        let stream = match Stream::new(&core, "vrover-capture", props) {
            Ok(s) => s,
            Err(e) => return fail(&latest, format!("Stream::new: {e}")),
        };

        let latest_proc = Arc::clone(&latest);
        let latest_param = Arc::clone(&latest);
        let _listener = match stream
            .add_local_listener::<()>()
            .param_changed(move |_, _, id, param| {
                // The node sends its fixated Format here. Parse the geometry so
                // decode_frame knows the real width/height (the portal-reported
                // size is only a hint).
                let Some(param) = param else {
                    return;
                };
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                    return;
                };
                if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                    return;
                }
                let mut info = VideoInfoRaw::new();
                if info.parse(param).is_err() {
                    return;
                }
                let size = info.size();
                if let Ok(mut g) = latest_param.lock() {
                    g.dims = Some((size.width, size.height));
                }
            })
            .process(move |s, _| {
                decode_frame(s, &latest_proc);
            })
            .register()
        {
            Ok(l) => l,
            Err(e) => return fail(&latest, format!("listener register: {e}")),
        };

        // Build a real SPA EnumFormat POD: offer BGRx/BGRA raw video over a size
        // range (defaulting to the portal-reported geometry), any framerate. The
        // node fixates this; the chosen size arrives via `param_changed` above.
        let obj = pipewire::spa::pod::object!(
            SpaTypes::ObjectParamFormat,
            ParamType::EnumFormat,
            pipewire::spa::pod::property!(
                FormatProperties::MediaType,
                Id,
                MediaType::Video
            ),
            pipewire::spa::pod::property!(
                FormatProperties::MediaSubtype,
                Id,
                MediaSubtype::Raw
            ),
            pipewire::spa::pod::property!(
                FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                VideoFormat::BGRx,
                VideoFormat::BGRx,
                VideoFormat::BGRA
            ),
            pipewire::spa::pod::property!(
                FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                Rectangle { width: w, height: h },
                Rectangle { width: 1, height: 1 },
                Rectangle { width: 4096, height: 4096 }
            ),
            pipewire::spa::pod::property!(
                FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                Fraction { num: 0, denom: 1 },
                Fraction { num: 0, denom: 1 },
                Fraction { num: 1000, denom: 1 }
            ),
        );
        let values: Vec<u8> = match PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(obj),
        ) {
            Ok((cursor, _len)) => cursor.into_inner(),
            Err(e) => return fail(&latest, format!("format pod serialize: {e}")),
        };
        let Some(format_pod) = Pod::from_bytes(&values) else {
            return fail(&latest, "format pod from_bytes failed");
        };
        let mut params = [format_pod];
        if let Err(e) = stream.connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        ) {
            return fail(&latest, format!("stream.connect: {e}"));
        }

        drop(_lock);
        // Serve pause/resume commands until the source is dropped (the sender
        // drops → recv returns Err → we exit, tearing down the stream + loop).
        while let Ok(Cmd::Active(active)) = rx.recv() {
            let _g = tl.lock();
            let _ = stream.set_active(active);
        }
    });

    (worker, tx)
}

/// Decode the first data plane of the dequeued buffer as BGRx/BGRA → BGRA [`Frame`].
/// Only the mmap'd pointer path (`SPA_DATA_MemPtr`) is handled; DMA-BUF is TODO(host).
fn decode_frame(stream: &pipewire::stream::StreamRef, latest: &Arc<Mutex<Shared>>) {
    // Decode against the negotiated geometry (set by param_changed). If it
    // hasn't arrived yet, skip this cycle.
    let (w, h) = match latest.lock().ok().and_then(|g| g.dims) {
        Some(d) => d,
        None => return,
    };
    let Some(mut buf) = stream.dequeue_buffer() else {
        return;
    };
    let Some(data) = buf.datas_mut().get_mut(0) else {
        return;
    };
    let Some(bytes) = data.data() else {
        return; // not a mmap'd pointer plane (e.g. DMA-BUF) — TODO(host)
    };
    let need = (w as usize)
        .saturating_mul(h as usize)
        .saturating_mul(4);
    if bytes.len() < need {
        return;
    }
    // `buf` drops at scope end → the buffer returns to the producer (no
    // backpressure), so the compositor keeps its own cadence; we only do the
    // expensive copy/decode when the TARGET_FPS throttle allows.
    let now = Instant::now();
    let mut g = match latest.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if g.last_decode
        .map(|t| now.duration_since(t) < MIN_FRAME_INTERVAL)
        .unwrap_or(false)
    {
        return; // too soon since the last decoded frame — drop this one
    }
    g.last_decode = Some(now);
    // Reuse the buffer (resize is a no-op after the first frame → no per-frame
    // ~39MB allocation).
    g.frame_buf.resize(need, 0);
    g.frame_buf.copy_from_slice(&bytes[..need]);
    // BGRx has an undefined pad byte; force opaque alpha for both BGRx and BGRA.
    for px in g.frame_buf.chunks_exact_mut(4) {
        px[3] = 255;
    }
    g.frame_ready = true;
}

fn fail(latest: &Arc<Mutex<Shared>>, msg: impl Into<String>) {
    if let Ok(mut g) = latest.lock() {
        g.error = Some(msg.into());
    }
}

fn session_err(e: ashpd::Error) -> DriverError {
    DriverError::Session(e.to_string())
}
