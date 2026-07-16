//! scout_debug — dedicated diagnostic for the omni-scout `/audio` connection.
//!
//! Connects to scout, streams windows, and reports:
//!   - total windows received + bytes received
//!   - effective data rate (KB/s) vs expected (32 KB/s @ 16k mono s16le)
//!   - window arrival cadence (windows/s vs expected ~31/s @ 512 samples)
//!   - ring fill ratio (if consuming)
//!
//! Run: cargo run -p audio-aura-asr --example scout_debug -- 127.0.0.1:7879

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audio_aura_asr::scout::ScoutAudioSource;

fn main() {
    let addr = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SCOUT_ADDR").ok())
        .unwrap_or_else(|| "127.0.0.1:7878".to_string());
    let window = 512usize;

    println!("● scout_debug — connecting to {addr}/audio (window={window})");
    println!("  expected rate: ~31 windows/s, ~32 KB/s (16k mono s16le)\n");

    let count = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    let count_t = Arc::clone(&count);
    let bytes_t = Arc::clone(&bytes);
    let src = ScoutAudioSource::new(addr.clone(), window);
    std::thread::spawn(move || {
        src.stream(
            move |win| {
                count_t.fetch_add(1, Ordering::Relaxed);
                bytes_t.fetch_add((win.len() * 2) as u64, Ordering::Relaxed);
            },
            Duration::from_secs(2),
        );
    });

    // Periodic report every 2s.
    let start = Instant::now();
    let mut last_report = Instant::now();
    let mut last_count = 0u64;
    let mut last_bytes = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if last_report.elapsed() >= Duration::from_secs(2) {
            let now_count = count.load(Ordering::Relaxed);
            let now_bytes = bytes.load(Ordering::Relaxed);
            let dt = last_report.elapsed().as_secs_f64();
            let d_count = now_count - last_count;
            let d_bytes = now_bytes - last_bytes;
            let total_dt = start.elapsed().as_secs_f64();
            println!(
                "@{total_dt:>5.1}s | win/s={d_win:>6.0} (expect ~31) | KB/s={kb:>6.1} (expect ~32) | total win={now_count} bytes={now_bytes}",
                d_win = d_count as f64 / dt,
                kb = d_bytes as f64 / dt / 1024.0,
            );
            last_report = Instant::now();
            last_count = now_count;
            last_bytes = now_bytes;
        }
    }
}
