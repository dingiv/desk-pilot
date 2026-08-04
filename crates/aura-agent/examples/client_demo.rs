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
            AsrSegment::Interim { seq, partial, .. } => {
                println!("[{t}] interim  #{seq} | {partial}")
            }
            AsrSegment::CalibratedInterim { seq, calibrated } => {
                println!("[{t}] calibrated #{seq} | {calibrated}")
            }
            AsrSegment::Final { seq, raw_text, calibrated, intent, route_ms, .. } => {
                println!(
                    "[{t}] FINAL    #{seq} ({}, {route_ms:.0}ms) | raw={raw_text:?} → {calibrated:?}",
                    intent,
                );
            }
            AsrSegment::Correction { seq, raw, corrected } => {
                println!("[{t}] CORRECT  #{seq} | {raw:?} → {corrected:?}");
            }
        }
    }
    Ok(())
}
