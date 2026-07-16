//! Media-backend sanity check: open a file, capture 2 video frames (confirm they
//! differ → content is advancing) and drain ~2 s of audio (confirm ~32 KB/s).
//!
//!   cargo run -p vrover-drivers --example media_play -- /home/host/Videos/bilibili_demo.mp4

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use vrover_drivers::audio::AudioSource;
use vrover_drivers::backends::media::{MediaAudioSource, MediaVideoSource};
use vrover_drivers::CaptureSource;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/host/Videos/bilibili_demo.mp4".into());

    // ── video ──────────────────────────────────────────────────────────────
    let mut v = match MediaVideoSource::new(&path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[media_play] video open failed: {e}");
            std::process::exit(2);
        }
    };
    eprintln!("[media_play] video dims = {:?}", v.dims());

    // wait for first frame, then capture two spaced ~150ms apart
    let dl = Instant::now() + Duration::from_secs(10);
    while Instant::now() < dl && v.capture().is_err() {
        thread::sleep(Duration::from_millis(20));
    }
    let f1 = v.capture().expect("first frame");
    thread::sleep(Duration::from_millis(150));
    let f2 = v.capture().expect("second frame");
    eprintln!(
        "[media_play] frame {}x{} {}B; bytes differing between f1/f2: {}/{} ({:.0}%)",
        f1.width,
        f1.height,
        f1.bgra.len(),
        f1.bgra.iter().zip(f2.bgra.iter()).filter(|(a, b)| a != b).count(),
        f1.bgra.len(),
        100.0 * f1.bgra.iter().zip(f2.bgra.iter()).filter(|(a, b)| a != b).count() as f64 / f1.bgra.len() as f64
    );

    // ── audio ──────────────────────────────────────────────────────────────
    let a = MediaAudioSource::new(&path).expect("audio open");
    eprintln!("[media_play] audio format = {:?}", a.format());
    let sub = a.subscribe().expect("subscribe");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut total = 0u64;
    while Instant::now() < deadline {
        match sub.recv_timeout(Duration::from_millis(200)) {
            Ok(b) => total += b.len() as u64,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    eprintln!(
        "[media_play] audio: {total} bytes in ~2s = {:.0} bytes/s (expect ~32000)",
        total as f64 / 2.0
    );
}
