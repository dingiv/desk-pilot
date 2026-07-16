//! One-shot capture demo: negotiate a PipeWire ScreenCast session, wait for the
//! first frame, and write it to a PNG at a path given as argv[1] (default
//! `/tmp/vrover-shot/cap.png`).
//!
//! Run (real backend + live Wayland session):
//!   cargo run -p scout-drivers --example capture_one --features pipewire -- /tmp/x.png
//!
//! The portal may pop a "select what to share" dialog on the desktop — approve it.

use std::time::{Duration, Instant};

use scout_drivers::CaptureSource;
use scout_drivers::backends::pipewire::PipeWireSource;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/vrover-shot/cap.png".to_string());

    eprintln!("[capture_one] negotiating ScreenCast session via portal…");
    let mut src = match PipeWireSource::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[capture_one] session negotiation failed: {e}");
            std::process::exit(2);
        }
    };
    if let Some((w, h)) = src.size() {
        eprintln!("[capture_one] portal reported stream size {w}x{h}");
    } else {
        eprintln!("[capture_one] no size from portal yet (will learn from first buffer)");
    }

    // Poll for the first frame. The pipewire worker is on its own thread; a frame
    // arrives once the portal starts streaming and we negotiate a format.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut frame = None;
    while Instant::now() < deadline {
        match src.capture() {
            Ok(f) => {
                frame = Some(f);
                break;
            }
            Err(e) => {
                // Expected ("no frame yet") until the first buffer lands.
                eprintln!("[capture_one] waiting for first frame… ({e})");
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    let frame = match frame {
        Some(f) => f,
        None => {
            eprintln!("[capture_one] timed out waiting for a frame");
            std::process::exit(3);
        }
    };

    let png = frame.to_png();
    if let Err(e) = std::fs::write(&out, &png) {
        eprintln!("[capture_one] failed to write {out}: {e}");
        std::process::exit(4);
    }
    eprintln!(
        "[capture_one] OK: {}x{} -> {} ({} bytes)",
        frame.width,
        frame.height,
        out,
        png.len()
    );
}
