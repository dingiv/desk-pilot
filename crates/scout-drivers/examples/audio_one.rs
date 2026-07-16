//! One-shot audio capture demo: connect to the local PipeWire daemon, subscribe
//! to the microphone (16 kHz mono S16LE), and drain ~3 s of PCM — reporting the
//! negotiated format + the achieved byte rate vs. the expected 32 KB/s.
//!
//! Run (real backend + PipeWire running):
//!   cargo run -p scout-drivers --example audio_one --features pipewire
//!
//! A real input device isn't required to verify the *pipeline* — even with no
//! physical mic the graph still delivers buffers (silence), so the byte rate
//! should still land near 32 KB/s if the 16 kHz negotiation took.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use scout_drivers::audio::AudioSource;
use scout_drivers::backends::pipewire::PipeWireAudioSource;

const DURATION_SECS: f64 = 3.0;

fn main() {
    eprintln!("[audio_one] connecting to PipeWire, capturing default mic…");
    let src = match PipeWireAudioSource::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[audio_one] connect failed: {e}");
            std::process::exit(2);
        }
    };

    // Subscribe before the format is necessarily known; param_changed fills it in.
    let sub = match src.subscribe() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[audio_one] subscribe failed: {e}");
            std::process::exit(2);
        }
    };

    let deadline = Instant::now() + Duration::from_secs_f64(DURATION_SECS);
    let mut total = 0u64;
    let mut chunks = 0u64;
    while Instant::now() < deadline {
        match sub.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                if chunks == 0 {
                    if let Some(f) = src.format() {
                        eprintln!(
                            "[audio_one] negotiated: {} Hz, {} ch, S16LE",
                            f.rate, f.channels
                        );
                    }
                }
                total += bytes.len() as u64;
                chunks += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("[audio_one] stream disconnected early");
                break;
            }
        }
    }

    let got_rate = total as f64 / DURATION_SECS;
    eprintln!(
        "[audio_one] captured {chunks} chunks, {total} bytes in ~{DURATION_SECS:.0}s (~{got_rate:.0} bytes/s)"
    );
    match src.format() {
        Some(f) => {
            let expect = f.rate as f64 * f.channels as f64 * 2.0; // S16 = 2 bytes/sample
            eprintln!(
                "[audio_one] expected ~{expect:.0} bytes/s at {} Hz x {} ch S16LE — {}",
                f.rate,
                f.channels,
                if (got_rate - expect).abs() / expect < 0.1 {
                    "OK (within 10%)"
                } else {
                    "MISMATCH"
                }
            );
        }
        None => eprintln!("[audio_one] WARNING: format never negotiated (no audio graph?)"),
    }
}
