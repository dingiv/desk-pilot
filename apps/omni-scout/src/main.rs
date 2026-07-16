//! omni-scout — the Visual Scout **capture daemon**.
//!
//! Holds ONE persistent screen source + ONE persistent audio source, then serves
//! both over HTTP so other apps can grab a screenshot / audio without each
//! re-negotiating capture. The sources are backend-agnostic trait objects, so the
//! same daemon serves either:
//! - the **real** PipeWire backends (ScreenCast portal + mic), or
//! - the **mock** file-backed `media` backends (`--mock <file>`): decode a media
//!   file to BGRA frames + 16 kHz mono S16LE via ffmpeg, no portal / no mic.
//!
//! ```text
//! GET /health  -> {"ok":true,"service":"omni-scout","screen_active":bool,"audio":bool,"audio_subscribers":N}
//! GET /info    -> {"width":..,"height":..,"audio":{"rate":16000,"channels":1,"format":"S16LE"}}
//! GET /frame   -> image/png  (latest frame; resumes the screen stream if it was idle-paused)
//! GET /audio   -> audio/pcm, Transfer-Encoding: chunked  (16 kHz mono S16LE; resumes the audio stream)
//! ```
//!
//! **Idle-aware:** both streams are paused when nobody is pulling from them
//! (`set_active(false)` → the producer stops, ~zero capture cost) and resumed on
//! the next request/connect. Audio is full-rate + gap-free while a client listens
//! (dropping samples would garble speech recognition) — the only laziness is
//! "off when nobody's listening".
//!
//! **Audio is optional:** if the audio source fails to init (no mic, or a mock
//! file with no audio track), the daemon continues in screen-only mode
//! (`audio:false`, `/audio` → 503).
//!
//! **Single instance:** acquires an exclusive `flock` on
//! `/run/omni-scout.lock` (falls back to `$XDG_RUNTIME_DIR` if `/run` isn't
//! writable); exits if another instance holds it.
//!
//! Reuses [`vrover_drivers`]'s `CaptureSource` + `AudioSource` traits (PipeWire
//! and `media` backends).

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

use vrover_drivers::audio::AudioSource;
use vrover_drivers::backends::media::{MediaAudioSource, MediaVideoSource};
use vrover_drivers::backends::pipewire::{PipeWireAudioSource, PipeWireSource};
use vrover_drivers::CaptureSource;
use vrover_drivers::mock::MockCaptureSource;

mod server;

const LOCK_PATH: &str = "/run/omni-scout.lock";

/// Screen source type held by the server (see `server::Screen`).
type ScreenBox = Box<dyn CaptureSource + Send>;
/// Audio source type held by the server (see `server::Audio`).
type AudioArc = Arc<dyn AudioSource + Send + Sync>;

fn main() {
    let args = Args::parse();
    acquire_singleton_lock(); // exits(3) if another instance holds the lock

    let (src, audio) = if let Some(file) = &args.mock {
        build_mock(file, &args)
    } else if args.mock_audio.is_some() {
        // `--mock-audio <file>` alone (no --mock): mock the AUDIO only, no video/portal. Lets us
        // feed a pure-audio file (m4a/wav/mp3) as a simulated mic without a video track.
        build_mock_audio_only(args.mock_audio.as_deref().unwrap())
    } else {
        build_real(args.audio_only)
    };

    let mode = if args.mock_audio.is_some() && args.mock.is_none() {
        " [MOCK-AUDIO]"
    } else if args.audio_only {
        " (audio-only)"
    } else if args.mock.is_some() {
        " [MOCK]"
    } else {
        ""
    };
    let srv = server::HttpServer::new(src, audio);
    eprintln!(
        "[omni-scout] serving on http://{}:{}   (GET /health | /info | /frame | /audio{}; streams pause when idle)",
        args.host,
        args.port,
        mode
    );
    eprintln!("[omni-scout] Ctrl+C to stop.");

    if let Err(e) = srv.serve(&args.host, args.port) {
        eprintln!("[omni-scout] server error: {e}");
        std::process::exit(1);
    }
}

/// Mock AUDIO only (no video, no portal): feed a pure-audio file (m4a/wav/mp3) as a simulated mic.
/// The screen is a stub solid frame (nobody reads /frame in this mode). Used to replay recordings
/// for reproducible ASR testing without a real microphone.
fn build_mock_audio_only(audio_path: &str) -> (Arc<Mutex<ScreenBox>>, Option<AudioArc>) {
    eprintln!("[omni-scout] MOCK-AUDIO mode: decoding {audio_path} via ffmpeg (no video/portal)");
    let src: Arc<Mutex<ScreenBox>> = Arc::new(Mutex::new(Box::new(
        MockCaptureSource::solid(320, 240, 0, 0, 0),
    )));
    let audio = match MediaAudioSource::new(audio_path) {
        Ok(a) => {
            eprintln!("[omni-scout] mock audio ready ({audio_path})");
            Some(Arc::new(a) as AudioArc)
        }
        Err(e) => {
            eprintln!("[omni-scout] mock audio unavailable: {e}");
            None
        }
    };
    (src, audio)
}

/// Real PipeWire sources: ScreenCast portal (may prompt to pick a screen) + mic.
/// Mic is best-effort: a missing/broken mic degrades to screen-only.
fn build_real(audio_only: bool) -> (Arc<Mutex<ScreenBox>>, Option<AudioArc>) {
    if audio_only {
        eprintln!("[omni-scout] audio-only mode — skipping ScreenCast portal");
        let audio = match PipeWireAudioSource::new() {
            Ok(a) => {
                eprintln!("[omni-scout] mic source ready (16 kHz mono S16LE requested)");
                Some(Arc::new(a) as AudioArc)
            }
            Err(e) => {
                eprintln!("[omni-scout] mic source unavailable: {e}");
                std::process::exit(2);
            }
        };
        return (Arc::new(Mutex::new(Box::new(vrover_drivers::mock::MockCaptureSource::solid(1, 1, 0, 0, 0)))), audio);
    }
    eprintln!(
        "[omni-scout] negotiating PipeWire ScreenCast session (the portal may prompt to pick a screen)…"
    );
    let pw = match PipeWireSource::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[omni-scout] capture session failed: {e}");
            std::process::exit(2);
        }
    };
    match pw.size() {
        Some((w, h)) => eprintln!("[omni-scout] stream size {w}x{h}"),
        None => eprintln!("[omni-scout] size unknown yet (learned on the first frame)"),
    }
    let src: Arc<Mutex<ScreenBox>> = Arc::new(Mutex::new(Box::new(pw)));

    let audio = match PipeWireAudioSource::new() {
        Ok(a) => {
            eprintln!("[omni-scout] mic source ready (16 kHz mono S16LE requested)");
            Some(Arc::new(a) as AudioArc)
        }
        Err(e) => {
            eprintln!("[omni-scout] mic source unavailable: screen-only mode ({e})");
            None
        }
    };
    (src, audio)
}

/// File-backed mock sources (no portal, no mic): decode a media file to BGRA
/// frames + 16 kHz mono S16LE via ffmpeg. `--mock <file>` feeds both; the
/// optional `--mock-video` / `--mock-audio` overrides pick separate files.
/// Audio is best-effort (a file with no audio track degrades to screen-only).
fn build_mock(file: &str, args: &Args) -> (Arc<Mutex<ScreenBox>>, Option<AudioArc>) {
    let video_path = args.mock_video.as_deref().unwrap_or(file);
    let audio_path = args.mock_audio.as_deref().unwrap_or(file);
    eprintln!("[omni-scout] MOCK mode: decoding {video_path} (video) via ffmpeg");

    let mv = match MediaVideoSource::new(video_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[omni-scout] mock video open failed: {e}");
            std::process::exit(2);
        }
    };
    eprintln!("[omni-scout] mock video ready {:?}", mv.dims());
    let src: Arc<Mutex<ScreenBox>> = Arc::new(Mutex::new(Box::new(mv)));

    let audio = match MediaAudioSource::new(audio_path) {
        Ok(a) => {
            eprintln!("[omni-scout] mock audio ready ({audio_path})");
            Some(Arc::new(a) as AudioArc)
        }
        Err(e) => {
            eprintln!("[omni-scout] mock audio unavailable: screen-only mock ({e})");
            None
        }
    };
    (src, audio)
}

/// Acquire an exclusive, non-blocking `flock` on the singleton lock. Auto-released
/// when the process exits. Exits(3) if another instance already holds it.
fn acquire_singleton_lock() {
    // Try the system-wide path first; if it isn't writable (unprivileged user),
    // fall back to $XDG_RUNTIME_DIR so the daemon still gets a single-instance guard.
    let xdg = std::env::var("XDG_RUNTIME_DIR");
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(LOCK_PATH);
    let path = if f.is_ok() {
        LOCK_PATH.to_string()
    } else {
        let dir = xdg.as_deref().unwrap_or("/run/user/1000");
        let p = format!("{dir}/omni-scout.lock");
        eprintln!("[omni-scout] {LOCK_PATH} not writable; using {p}");
        f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&p);
        p
    };
    let mut f = match f {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[omni-scout] cannot open lock {path}: {e}");
            std::process::exit(3);
        }
    };
    // LOCK_EX | LOCK_NB: exclusive, non-blocking. The lock auto-releases on exit.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        eprintln!(
            "[omni-scout] another omni-scout instance is running (lock {path} held) — exiting."
        );
        std::process::exit(3);
    }
    let _ = f.set_len(0);
    let _ = f.write_all(format!("{}\n", std::process::id()).as_bytes());
    eprintln!(
        "[omni-scout] singleton lock acquired: {path} (pid {})",
        std::process::id()
    );
    // Leak the file so the lock lives for the process lifetime (released on exit).
    std::mem::forget(f);
}

/// Parsed CLI args. A leading literal `--` (pnpm forwarding) is stripped.
struct Args {
    host: String,
    port: u16,
    /// Enable mock mode, feeding this media file to both video + audio.
    mock: Option<String>,
    /// Override the mock video file (defaults to `mock`).
    mock_video: Option<String>,
    /// Override the mock audio file (defaults to `mock`).
    mock_audio: Option<String>,
    audio_only: bool,
}

impl Args {
    fn parse() -> Self {
        let mut host: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut mock: Option<String> = None;
        let mut mock_video: Option<String> = None;
        let mut mock_audio: Option<String> = None;
        let mut audio_only = false;
        let mut help = false;
        let mut it = std::env::args().skip(1).filter(|a| a.as_str() != "--");
        while let Some(a) = it.next() {
            match a.as_str() {
                "-h" | "--help" => help = true,
                "--host" => host = it.next(),
                "--port" => port = it.next().and_then(|p| p.parse().ok()),
                "--mock" => mock = it.next(),
                "--mock-video" => mock_video = it.next(),
                "--mock-audio" => mock_audio = it.next(),
                "--audio-only" => audio_only = true,
                _ => {}
            }
        }
        if help {
            eprintln!(
                "omni-scout — Visual Scout capture daemon\n\n\
                 Usage:\n  omni-scout [--host <host>] [--port <port>] [--mock <file>]\n\n\
                 Options:\n  --host <host>  Bind host (default $SCOUT_HOST or 127.0.0.1)\n\
                 \x20 --port <port>  Bind port (default $SCOUT_PORT or 7878)\n\
                 \x20 --mock <file>  Mock mode: decode <file> to frames + audio (no portal/mic)\n\
                 \x20 --mock-video <f> / --mock-audio <f>  Separate mock files per stream\n\
                 \x20 --audio-only        Only capture mic audio — skip the ScreenCast portal (zero GPU)\n\
                 \x20 -h, --help          Show this help"
            );
            std::process::exit(0);
        }
        let host = host.unwrap_or_else(|| {
            std::env::var("SCOUT_HOST").unwrap_or_else(|_| "127.0.0.1".into())
        });
        let port = port.unwrap_or_else(|| {
            std::env::var("SCOUT_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(7878)
        });
        Args {
            host,
            port,
            mock,
            mock_video,
            mock_audio,
            audio_only,
        }
    }
}
