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
//! Reuses [`scout_drivers`]'s `CaptureSource` + `AudioSource` traits (PipeWire
//! and `media` backends).

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

use tracing::{error, info, warn};

use scout_drivers::audio::AudioSource;
use scout_drivers::backends::media::{MediaAudioSource, MediaVideoSource, MockAudioDirSource};
use scout_drivers::backends::pipewire::{PipeWireAudioSource, PipeWireSource};
use scout_drivers::CaptureSource;
use scout_drivers::mock::MockCaptureSource;

mod server;

const LOCK_PATH: &str = "/run/omni-scout.lock";

/// Screen source type held by the server (see `server::Screen`).
type ScreenBox = Box<dyn CaptureSource + Send>;
/// Audio source type held by the server (see `server::Audio`).
type AudioArc = Arc<dyn AudioSource + Send + Sync>;

fn main() {
    // Init-stage side effect, first thing in main: the process-wide tracing subscriber
    // (dev: human-readable; release: JSON lines; RUST_LOG filter, default info).
    shared::init_tracing();
    let args = Args::parse();
    acquire_singleton_lock(); // exits(3) if another instance holds the lock

    // Resolve mock/mock_audio paths via shared FileLoader: bare filenames like
    // `hungry_snake.m4a` resolve to this crate's `assets/` dir (declared in Cargo.toml
    // `[package.metadata.shared]`). Full paths work too.
    let fs = shared::loader!();
    let resolve = |path: &str| -> String {
        // Try as-is first (absolute / cwd-relative).
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
        // Try as a bare filename in the ASSETS namespace.
        let resolved = fs.resolve(&format!("ASSETS::{path}"));
        match resolved {
            Some(p) => p.to_string_lossy().into_owned(),
            None => path.to_string(), // let ffmpeg error naturally
        }
    };
    let mock = args.mock.as_deref().map(|p| resolve(p));
    let mock_audio = args.mock_audio.as_deref().map(|p| resolve(p));

    let (src, audio) = if let Some(file) = &mock {
        build_mock(file, &args)
    } else if mock_audio.is_some() {
        // mock_audio is now a DIRECTORY (not a single file). Default to the ASSETS/mock-audio
        // namespace if the user passed `--mock-audio` without a path.
        let dir = mock_audio.as_deref().unwrap_or("mock-audio");
        let resolved_dir = if std::path::Path::new(dir).exists() {
            dir.to_string()
        } else {
            fs.resolve(&format!("ASSETS::{dir}"))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string())
        };
        build_mock_audio_dir(&resolved_dir)
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
    info!(
        host = %args.host,
        port = args.port,
        mode,
        "serving (GET /health | /info | /frame | /audio; streams pause when idle) — Ctrl+C to stop"
    );

    if let Err(e) = srv.serve(&args.host, args.port) {
        error!(error = %e, "server error");
        std::process::exit(1);
    }
}

/// Mock AUDIO from a directory (no video, no portal): each subscriber gets a randomly
/// chosen file from `dir`, decoded on-demand (no preheating). The screen is a stub.
fn build_mock_audio_dir(dir: &str) -> (Arc<Mutex<ScreenBox>>, Option<AudioArc>) {
    let src: Arc<Mutex<ScreenBox>> = Arc::new(Mutex::new(Box::new(
        MockCaptureSource::solid(320, 240, 0, 0, 0),
    )));
    let audio = match MockAudioDirSource::new(dir) {
        Ok(a) => {
            info!(dir = %dir, "mock-audio dir ready");
            Some(Arc::new(a) as AudioArc)
        }
        Err(e) => {
            warn!(error = %e, "mock-audio dir unavailable");
            None
        }
    };
    (src, audio)
}

/// Real PipeWire sources: ScreenCast portal (may prompt to pick a screen) + mic.
/// Mic is best-effort: a missing/broken mic degrades to screen-only.
fn build_real(audio_only: bool) -> (Arc<Mutex<ScreenBox>>, Option<AudioArc>) {
    if audio_only {
        info!("audio-only mode — skipping ScreenCast portal");
        let audio = match PipeWireAudioSource::new() {
            Ok(a) => {
                info!("mic source ready (16 kHz mono S16LE requested)");
                Some(Arc::new(a) as AudioArc)
            }
            Err(e) => {
                error!(error = %e, "mic source unavailable");
                std::process::exit(2);
            }
        };
        return (Arc::new(Mutex::new(Box::new(scout_drivers::mock::MockCaptureSource::solid(1, 1, 0, 0, 0)))), audio);
    }
    info!("negotiating PipeWire ScreenCast session (the portal may prompt to pick a screen)…");
    let pw = match PipeWireSource::new() {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "capture session failed");
            std::process::exit(2);
        }
    };
    match pw.size() {
        Some((w, h)) => info!(w, h, "stream size"),
        None => info!("size unknown yet (learned on the first frame)"),
    }
    let src: Arc<Mutex<ScreenBox>> = Arc::new(Mutex::new(Box::new(pw)));

    let audio = match PipeWireAudioSource::new() {
        Ok(a) => {
            info!("mic source ready (16 kHz mono S16LE requested)");
            Some(Arc::new(a) as AudioArc)
        }
        Err(e) => {
            warn!(error = %e, "mic source unavailable: screen-only mode");
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
    info!(path = %video_path, "MOCK mode: decoding video via ffmpeg");

    let mv = match MediaVideoSource::new(video_path) {
        Ok(v) => v,
        Err(e) => {
            error!(error = %e, "mock video open failed");
            std::process::exit(2);
        }
    };
    info!(dims = ?mv.dims(), "mock video ready");
    let src: Arc<Mutex<ScreenBox>> = Arc::new(Mutex::new(Box::new(mv)));

    let audio = match MediaAudioSource::new(audio_path) {
        Ok(a) => {
            info!(path = %audio_path, "mock audio ready");
            Some(Arc::new(a) as AudioArc)
        }
        Err(e) => {
            warn!(error = %e, "mock audio unavailable: screen-only mock");
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
        info!(fallback = %p, "{LOCK_PATH} not writable; using XDG runtime dir");
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
            error!(path = %path, error = %e, "cannot open lock");
            std::process::exit(3);
        }
    };
    // LOCK_EX | LOCK_NB: exclusive, non-blocking. The lock auto-releases on exit.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        error!(lock = %path, "another omni-scout instance is running — exiting");
        std::process::exit(3);
    }
    let _ = f.set_len(0);
    let _ = f.write_all(format!("{}\n", std::process::id()).as_bytes());
    info!(lock = %path, pid = std::process::id(), "singleton lock acquired");
    // Leak the file so the lock lives for the process lifetime (released on exit).
    std::mem::forget(f);
}

/// Runtime config (`CONF::scout.json` via the shared FileLoader — dev: this crate's dir;
/// prod: the unified `~/.desk-pilot/` folder). Every field is optional; precedence is
/// CLI arg > env var > config file > built-in default.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct ScoutConf {
    host: Option<String>,
    port: Option<u16>,
    audio_only: Option<bool>,
    /// Default mock media file (CLI `--mock` overrides). Bare filenames resolve in ASSETS.
    mock: Option<String>,
    mock_video: Option<String>,
    mock_audio: Option<String>,
}

impl ScoutConf {
    /// Load `CONF::scout.json`. Missing file = all defaults; malformed = reported + ignored.
    fn load() -> Self {
        let fs = shared::loader!();
        match fs.read_str("CONF::scout.json") {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(conf) => {
                    let from = fs
                        .resolve("CONF::scout.json")
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "scout.json".into());
                    info!(path = %from, "conf loaded");
                    conf
                }
                Err(e) => {
                    warn!(error = %e, "scout.json parse error — using defaults");
                    Self::default()
                }
            },
            Err(_) => {
                info!("no scout.json — using built-in defaults");
                Self::default()
            }
        }
    }
}

/// CLI — high-frequency knobs only; the FULL config surface lives in `scout.json`
/// (see [`ScoutConf`]). Precedence: CLI > config file > built-in default.
#[derive(Debug, Default, clap::Parser)]
#[command(
    name = "omni-scout",
    about = "Visual Scout capture daemon — persistent screen + mic capture over HTTP",
    version
)]
struct Cli {
    /// Bind host
    #[arg(long)]
    host: Option<String>,
    /// Bind port
    #[arg(short, long)]
    port: Option<u16>,
    /// Mock mode: decode FILE to frames + audio (no portal/mic); bare names resolve in assets/
    #[arg(long, value_name = "FILE")]
    mock: Option<String>,
    /// Override the mock video file (defaults to --mock's FILE)
    #[arg(long, value_name = "FILE")]
    mock_video: Option<String>,
    /// Override the mock audio source — a directory of audio files (each subscriber gets a random
    /// one). With no arg, defaults to the `assets/mock-audio` dir.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "mock-audio")]
    mock_audio: Option<String>,
    /// Only capture mic audio — skip the ScreenCast portal (zero GPU)
    #[arg(long)]
    audio_only: bool,
}

/// Fully-resolved runtime settings (what `main` actually runs on).
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

/// Pure merge: CLI > `scout.json` > built-in default.
fn resolve(cli: Cli, conf: ScoutConf) -> Args {
    Args {
        host: cli.host.or(conf.host).unwrap_or_else(|| "127.0.0.1".into()),
        port: cli.port.or(conf.port).unwrap_or(7878),
        mock: cli.mock.or(conf.mock),
        mock_video: cli.mock_video.or(conf.mock_video),
        mock_audio: cli.mock_audio.or(conf.mock_audio),
        audio_only: cli.audio_only || conf.audio_only.unwrap_or(false),
    }
}

impl Args {
    fn parse() -> Self {
        resolve(<Cli as clap::Parser>::parse(), ScoutConf::load())
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve, Cli, ScoutConf};

    #[test]
    fn checked_in_scout_json_parses() {
        // Guard the dev runtime config against schema drift / typos.
        let s = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/scout.json"))
            .expect("apps/omni-scout/scout.json missing");
        let conf: ScoutConf = serde_json::from_str(&s).expect("scout.json must parse");
        assert_eq!(conf.port, Some(7878));
        assert_eq!(conf.mock, None, "checked-in config must not pin a mock file");
    }

    #[test]
    fn resolve_precedence_cli_over_conf_over_default() {
        let cli = Cli { port: Some(9000), mock: Some("cli.m4a".into()), ..Cli::default() };
        let conf = ScoutConf {
            host: Some("0.0.0.0".into()),
            port: Some(1234),
            audio_only: Some(true),
            mock: Some("conf.m4a".into()),
            ..ScoutConf::default()
        };
        let a = resolve(cli, conf);
        assert_eq!(a.host, "0.0.0.0", "file wins when CLI silent");
        assert_eq!(a.port, 9000, "CLI wins over file");
        assert_eq!(a.mock.as_deref(), Some("cli.m4a"));
        assert!(a.audio_only, "file flag applies when CLI flag absent");

        let d = resolve(Cli::default(), ScoutConf::default());
        assert_eq!((d.host.as_str(), d.port, d.audio_only), ("127.0.0.1", 7878, false));
    }
}
