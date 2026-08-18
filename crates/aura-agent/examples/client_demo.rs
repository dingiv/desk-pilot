//! Demo for `audio_aura_agent::client::AuraClient` — exercises BOTH planes against a running
//! aura-daemon:
//! - control plane: one-shot `GET /api/state` (settings snapshot);
//! - data plane:    `subscribe_segments()` — live recognition segments pushed directly.
//!
//! Run: `cargo run -p audio-aura-agent --example client_demo -- http://127.0.0.1:9091`
//! (Start aura-daemon first.)

use audio_aura_agent::client::AuraClient;
use audio_aura_agent::AsrSegment;
use futures::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9091".into());
    let client = AuraClient::new(&base)?;
    eprintln!("connecting to {base} …  Ctrl-C 结束");

    // ── control plane: settings snapshot ──
    if let Ok(s) = client.state().await {
        eprintln!(
            "[state] connected={} asr={}({}) llm={}:{} merge_gap={}s hotwords={}",
            s.connected,
            s.config.asr_backend,
            s.config.asr_provider,
            s.config.llm_kind,
            s.config.model,
            s.config.vad.merge_gap,
            s.hotwords.len(),
        );
    } else {
        eprintln!("daemon not reachable at {base} yet — data-plane stream will retry");
    }

    // ── data plane: live recognition segments (low-latency push) ──
    let segments = client.subscribe_segments();
    tokio::pin!(segments); // the async_stream is !Unpin — pin before .next()
    while let Some(seg) = segments.next().await {
        let t = chrono::Local::now().format("%H:%M:%S%.3f");
        match seg {
            AsrSegment::StreamFragment { window_id, segment_id, text, .. } => {
                println!("[{t}] stream    w{window_id}/s{segment_id} | {text}")
            }
            AsrSegment::BatchSegment { window_id, segment_id, text } => {
                println!("[{t}] batch-seg w{window_id}/s{segment_id} | {text}")
            }
            AsrSegment::BatchWindow { window_id, text } => {
                println!("[{t}] batch-win w{window_id} | {text}")
            }
            AsrSegment::SegmentCalibration { window_id, calibrated } => {
                println!("[{t}] seg-calib w{window_id} | {calibrated}")
            }
            AsrSegment::WindowCalibration { window_id, calibrated } => {
                println!("[{t}] FINAL     w{window_id} | {calibrated}")
            }
            AsrSegment::Correction { window_id, raw, corrected } => {
                println!("[{t}] CORRECT   w{window_id} | {raw:?} → {corrected:?}");
            }
        }
    }
    Ok(())
}
