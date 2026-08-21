//! Minimal HTTP server for the capture daemon. std-only (no framework): a few
//! GET routes, one thread per connection, `Connection: close`.
//!
//! Source-agnostic: holds the screen + audio sources as trait objects
//! (`dyn CaptureSource` / `dyn AudioSource`), so the same server serves either
//! the real PipeWire backends or the file-backed `media` mock (see `--mock`).
//!
//! **Demand-driven capture:**
//! - **Video** (`/frame`): the screen stream is paused when idle (no request for
//!   [`IDLE_TIMEOUT`]) and resumed on the next request, so the daemon costs
//!   ~zero capture CPU while nobody is asking for frames.
//! - **Audio** (`/audio`): the stream is paused whenever it has zero subscribers
//!   and resumed on connect. Audio can't be frame-rate-throttled (dropped samples
//!   garble speech recognition), so the only laziness is "off when nobody's
//!   listening" — every subscriber gets every buffer, full-rate.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use scout_drivers::audio::AudioSource;
use scout_drivers::{CaptureSource, InputSink, Key, UinputSink, UinputSinkBuilder};

/// Screen source: a (mockable) frame producer behind a mutex (capture is `&mut`).
type Screen = Arc<Mutex<Box<dyn CaptureSource + Send>>>;
/// Audio source: a (mockable) continuous-PCM producer, internally synchronized.
type Audio = Arc<dyn AudioSource + Send + Sync>;

/// Pause the screen stream after this long with no `/frame` request.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for a frame (boot negotiation, or post-resume freshness).
const FRAME_WAIT: Duration = Duration::from_millis(1000);
/// How long a `/audio` write may block before we declare the client dead.
const AUDIO_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Keep a `/audio` recv alive across quiet gaps (stream warming up) before giving up.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_secs(30);

/// Demand state shared between the HTTP handlers and the idle ticker.
struct Demand {
    last_request: Instant,
    /// Is the screen stream currently capturing (vs idle-paused)?
    screen_active: bool,
    /// Is the audio stream currently capturing (vs paused)?
    audio_active: bool,
}

pub struct HttpServer {
    src: Screen,
    /// Optional audio. `None` if audio capture failed at boot → screen-only mode.
    audio: Option<Audio>,
    demand: Arc<Mutex<Demand>>,
    /// 懒创建的内核虚拟键盘(uinput)—— `#del` 等经 `/inject/backspace` 注入退格。
    /// 首次注入时才 open /dev/uinput。
    inject: Arc<Mutex<Option<UinputSink>>>,
}

impl HttpServer {
    pub fn new(src: Screen, audio: Option<Audio>) -> Self {
        let demand = Arc::new(Mutex::new(Demand {
            last_request: Instant::now(),
            screen_active: true,
            audio_active: true,
        }));
        Self {
            src,
            audio,
            demand,
            inject: Arc::new(Mutex::new(None)),
        }
    }

    /// Bind + accept loop. Blocks for the daemon's lifetime; also starts the idle
    /// ticker that pauses the streams when no client is pulling.
    pub fn serve(self, host: &str, port: u16) -> std::io::Result<()> {
        let ticker_src = Arc::clone(&self.src);
        let ticker_audio = self.audio.clone();
        let ticker_demand = Arc::clone(&self.demand);
        std::thread::spawn(move || idle_ticker(ticker_src, ticker_audio, ticker_demand));

        let listener = TcpListener::bind((host, port))?;
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let src = Arc::clone(&self.src);
            let audio = self.audio.clone();
            let demand = Arc::clone(&self.demand);
            let inject = Arc::clone(&self.inject);
            std::thread::spawn(move || {
                let _ = handle(stream, &src, &audio, &demand, &inject);
            });
        }
        Ok(())
    }
}

/// Every second:
/// - if the screen stream is active but nobody has requested a frame for
///   [`IDLE_TIMEOUT`], pause it (producer stops pushing → ~zero capture cost);
/// - if the audio stream is active but has zero subscribers, pause it too.
fn idle_ticker(src: Screen, audio: Option<Audio>, demand: Arc<Mutex<Demand>>) {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let mut d = demand.lock().unwrap();
        if d.screen_active && d.last_request.elapsed() > IDLE_TIMEOUT {
            if let Some(s) = src.lock().ok() {
                s.set_active(false);
            }
            d.screen_active = false;
            tracing::info!(idle_s = IDLE_TIMEOUT.as_secs(), "screen paused — ~zero capture cost");
        }
        if let Some(a) = &audio {
            if d.audio_active && a.subscriber_count() == 0 {
                a.set_active(false);
                d.audio_active = false;
                tracing::info!("audio paused (no subscribers)");
            }
        }
    }
}

/// Mark the screen stream needed (resume if it was idle-paused, clearing any stale
/// frame), refresh the idle timer, then return a PNG — waiting for a fresh frame
/// if we just resumed.
fn ensure_active_and_capture(src: &Screen, demand: &Arc<Mutex<Demand>>) -> Option<Vec<u8>> {
    {
        let mut d = demand.lock().unwrap();
        if !d.screen_active {
            if let Some(s) = src.lock().ok() {
                s.set_active(true);
                s.clear_frame();
            }
            d.screen_active = true;
            tracing::info!("screen resumed (capture active)");
        }
        d.last_request = Instant::now();
    }
    let deadline = Instant::now() + FRAME_WAIT;
    loop {
        if let Some(mut s) = src.lock().ok() {
            if let Ok(f) = s.capture() {
                return Some(f.to_png());
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn handle(
    mut stream: TcpStream,
    src: &Screen,
    audio: &Option<Audio>,
    demand: &Arc<Mutex<Demand>>,
    inject: &Arc<Mutex<Option<UinputSink>>>,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    if n == 0 {
        return Ok(());
    }
    let req = std::str::from_utf8(&buf[..n]).unwrap_or("");
    let raw = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    let (path, query) = match raw.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (raw, None),
    };

    // /audio is a chunked streaming response — handle it out-of-band (it can't
    // return a fixed Content-Length body like the other routes).
    if path == "/audio" {
        // Client-requested push cadence: `?chunk_ms=N` aggregates source buffers into
        // N-ms HTTP chunks. Absent → one chunk per source buffer (the base quantum rate).
        let chunk_ms = query.and_then(|q| {
            q.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == "chunk_ms").then(|| v.parse().ok()).flatten()
            })
        });
        return serve_audio(stream, audio, demand, chunk_ms);
    }

    let (status, ctype, body) = route(path, query, src, audio, demand, inject);
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\n\r\n",
        len = body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    Ok(())
}

/// `GET /audio` — chunked, never-ending (until client closes) stream of raw PCM.
///
/// Each captured audio buffer is forwarded as one HTTP chunk
/// (`{hex-size}\r\n{bytes}\r\n`). The stream is 16 kHz mono S16LE, so the bytes
/// are directly consumable by a streaming ASR client. The source is resumed on
/// connect and paused again by the idle ticker once the subscription (and all
/// others) drops.
///
/// `chunk_ms` (from `?chunk_ms=N`) lets the client request a SLOWER push cadence:
/// source buffers are aggregated into ~N-ms HTTP chunks. The floor is the source's
/// own buffer size (`buffer_samples`) — a client can't ask for chunks SMALLER than
/// the capture quantum (optimization-1 frequency). `None` → one chunk per source
/// buffer (the base quantum rate).
fn serve_audio(
    mut stream: TcpStream,
    audio: &Option<Audio>,
    demand: &Arc<Mutex<Demand>>,
    chunk_ms: Option<u64>,
) -> std::io::Result<()> {
    let Some(a) = audio else {
        return write_text(&mut stream, 503, "no audio source (init failed)");
    };
    // Resume the source if it was idle-paused.
    a.set_active(true);
    if let Ok(mut d) = demand.lock() {
        d.audio_active = true;
    }
    let sub = match a.subscribe() {
        Ok(s) => s,
        Err(e) => return write_text(&mut stream, 503, &format!("audio subscribe failed: {e}")),
    };

    // Report the format so the client can decode (mock = always 16k/1/S16LE).
    let (rate, ch) = a.format().map(|f| (f.rate, f.channels)).unwrap_or((16000, 1));
    let aggregate_to = aggregation_bytes(rate, a.buffer_samples().unwrap_or(0), chunk_ms);
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: audio/pcm\r\n\
         Transfer-Encoding: chunked\r\n\
         Connection: close\r\n\
         X-Sample-Rate: {rate}\r\n\
         X-Channels: {ch}\r\n\
         X-Format: S16LE\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Cache-Control: no-store\r\n\r\n"
    );
    let _ = stream.set_write_timeout(Some(AUDIO_WRITE_TIMEOUT));
    stream.write_all(head.as_bytes())?;

    // subscription drops at end of scope → unsubscribed → ticker pauses the source.
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let bytes = match sub.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(b) => b,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Source quiet (or ended): flush any partial aggregation, keep the conn alive.
                if !acc.is_empty() {
                    if write_chunk(&mut stream, &acc).is_err() {
                        break; // client gone
                    }
                    acc.clear();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match aggregate_to {
            Some(target) => {
                acc.extend_from_slice(&bytes);
                if acc.len() >= target {
                    if write_chunk(&mut stream, &acc).is_err() {
                        break; // client gone
                    }
                    acc.clear();
                }
            }
            None => {
                // No client cadence request → forward each source buffer as one chunk.
                if write_chunk(&mut stream, &bytes).is_err() {
                    break; // client gone
                }
            }
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n"); // terminator (best-effort)
    Ok(())
}

/// Write one HTTP/1.1 chunked-encoding frame: `{hex-size}\r\n{payload}\r\n`.
fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    let line = format!("{:x}\r\n", bytes.len());
    stream.write_all(line.as_bytes())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

/// Bytes per aggregated HTTP chunk for a client-requested cadence (mono S16LE: samples × 2).
/// `None` chunk_ms → `None` (forward each source chunk as-is). The result is clamped UP to the
/// source's own buffer size and rounded to a whole multiple of it — a client can't push faster
/// than the capture quantum, so `chunk_ms` below the quantum becomes one source chunk.
fn aggregation_bytes(rate: u32, floor_samples: u32, chunk_ms: Option<u64>) -> Option<usize> {
    chunk_ms.map(|ms| {
        let per_ms = (rate as usize) * 2 / 1000; // mono S16LE bytes per ms
        let floor_bytes = floor_samples as usize * 2;
        let want = if floor_bytes > 0 {
            (per_ms * ms as usize).max(floor_bytes)
        } else {
            per_ms * ms as usize
        };
        if floor_bytes > 0 {
            want.div_ceil(floor_bytes) * floor_bytes
        } else {
            want
        }
    })
}

#[cfg(test)]
mod tests {
    use super::aggregation_bytes;

    #[test]
    fn no_client_cadence_forwards_source_chunks() {
        // chunk_ms None → None → forward each source buffer as-is.
        assert_eq!(aggregation_bytes(16000, 1024, None), None);
    }

    #[test]
    fn requested_cadence_aggregates_and_clamps_to_floor() {
        // quantum 1024 @ 16kHz = 64ms → 64ms = 1024 samples = 2048 bytes floor.
        // Request 128ms → 128ms = 2048 samples = 4096 bytes (2 source chunks).
        assert_eq!(aggregation_bytes(16000, 1024, Some(128)), Some(4096));
        // Request 256ms → 4096 samples = 8192 bytes (4 source chunks).
        assert_eq!(aggregation_bytes(16000, 1024, Some(256)), Some(8192));
        // Request 32ms → BELOW the 64ms floor → clamped to one source chunk (64ms).
        assert_eq!(aggregation_bytes(16000, 1024, Some(32)), Some(2048));
    }

    #[test]
    fn rounding_up_to_source_multiple() {
        // 100ms @ 16kHz = 1600 samples; floor 1024 → round up to 2048 samples = 4096 bytes.
        assert_eq!(aggregation_bytes(16000, 1024, Some(100)), Some(4096));
        // Unknown source (floor 0): exact ms→bytes, no rounding.
        assert_eq!(aggregation_bytes(16000, 0, Some(100)), Some(3200));
    }
}

fn route(
    path: &str,
    query: Option<&str>,
    src: &Screen,
    audio: &Option<Audio>,
    demand: &Arc<Mutex<Demand>>,
    inject: &Arc<Mutex<Option<UinputSink>>>,
) -> (&'static str, &'static str, Vec<u8>) {
    match path {
        "/health" => {
            let d = demand.lock().unwrap();
            json(
                200,
                format!(
                    "{{\"ok\":true,\"service\":\"omni-scout\",\"screen_active\":{},\"audio\":{},\"audio_subscribers\":{}}}",
                    d.screen_active,
                    audio.is_some(),
                    audio.as_ref().map(|a| a.subscriber_count()).unwrap_or(0),
                ),
            )
        }
        "/info" => {
            let (w, h) = src.lock().ok().and_then(|s| s.size()).unwrap_or((0, 0));
            let audio = match audio {
                Some(a) => match a.format() {
                    Some(f) => format!(
                        ",\"audio\":{{\"rate\":{},\"channels\":{},\"format\":\"S16LE\"}}",
                        f.rate, f.channels
                    ),
                    None => ",\"audio\":{\"negotiating\":true}".into(),
                },
                None => ",\"audio\":null".into(),
            };
            json(200, format!("{{\"width\":{w},\"height\":{h}{audio}}}"))
        }
        "/frame" => match ensure_active_and_capture(src, demand) {
            Some(p) => ("200 OK", "image/png", p),
            None => json(
                503,
                "{\"ok\":false,\"error\":\"frame not ready (timed out)\"}".to_string(),
            ),
        },
        // `#del`:注入 N 个 Backspace。用 uinput 内核虚拟键盘(硬件级,绕过
        // 输入法框架 —— Wayland 下 forwardKey 依赖 compositor 虚拟键盘协议)。
        "/inject/backspace" => {
            let count = query
                .and_then(|q| {
                    q.split('&').find_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        (k == "count").then(|| v.parse::<usize>().ok()).flatten()
                    })
                })
                .unwrap_or(1);
            let mut sink = inject.lock().unwrap();
            if sink.is_none() {
                match UinputSinkBuilder::new().build() {
                    Ok(s) => *sink = Some(s),
                    Err(e) => {
                        return json(
                            500,
                            format!("{{\"ok\":false,\"error\":\"uinput open failed: {e}\"}}"),
                        );
                    }
                }
            }
            for _ in 0..count {
                if let Err(e) = sink.as_mut().unwrap().tap_key(Key::Backspace) {
                    return json(
                        500,
                        format!("{{\"ok\":false,\"error\":\"inject failed: {e}\"}}"),
                    );
                }
            }
            json(200, format!("{{\"ok\":true,\"injected\":{count}}}"))
        }
        _ => json(404, "{\"ok\":false,\"error\":\"not found\"}".to_string()),
    }
}

fn write_text(stream: &mut TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    let status = match code {
        200 => "200 OK",
        404 => "404 Not Found",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        len = msg.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(msg.as_bytes())?;
    Ok(())
}

fn json(code: u16, body: String) -> (&'static str, &'static str, Vec<u8>) {
    let status = match code {
        200 => "200 OK",
        404 => "404 Not Found",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    };
    (status, "application/json", body.into_bytes())
}
