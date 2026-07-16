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

use vrover_drivers::audio::AudioSource;
use vrover_drivers::CaptureSource;

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
}

impl HttpServer {
    pub fn new(src: Screen, audio: Option<Audio>) -> Self {
        let demand = Arc::new(Mutex::new(Demand {
            last_request: Instant::now(),
            screen_active: true,
            audio_active: true,
        }));
        Self { src, audio, demand }
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
            std::thread::spawn(move || {
                let _ = handle(stream, &src, &audio, &demand);
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
            eprintln!(
                "[omni-scout] screen paused (idle {}s) — ~zero capture cost",
                IDLE_TIMEOUT.as_secs()
            );
        }
        if let Some(a) = &audio {
            if d.audio_active && a.subscriber_count() == 0 {
                a.set_active(false);
                d.audio_active = false;
                eprintln!("[omni-scout] audio paused (no subscribers)");
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
            eprintln!("[omni-scout] screen resumed (capture active)");
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
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    if n == 0 {
        return Ok(());
    }
    let req = std::str::from_utf8(&buf[..n]).unwrap_or("");
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    let path = path.split('?').next().unwrap_or(path);

    // /audio is a chunked streaming response — handle it out-of-band (it can't
    // return a fixed Content-Length body like the other routes).
    if path == "/audio" {
        return serve_audio(stream, audio, demand);
    }

    let (status, ctype, body) = route(path, src, audio, demand);
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
fn serve_audio(
    mut stream: TcpStream,
    audio: &Option<Audio>,
    demand: &Arc<Mutex<Demand>>,
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
    loop {
        let bytes = match sub.recv_timeout(AUDIO_RECV_TIMEOUT) {
            Ok(b) => b,
            Err(mpsc::RecvTimeoutError::Timeout) => continue, // keep connection alive
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // HTTP/1.1 chunk: hex size CRLF, payload, CRLF.
        let line = format!("{:x}\r\n", bytes.len());
        if stream.write_all(line.as_bytes()).is_err()
            || stream.write_all(&bytes).is_err()
            || stream.write_all(b"\r\n").is_err()
            || stream.flush().is_err()
        {
            break; // client gone
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n"); // terminator (best-effort)
    Ok(())
}

fn route(
    path: &str,
    src: &Screen,
    audio: &Option<Audio>,
    demand: &Arc<Mutex<Demand>>,
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
