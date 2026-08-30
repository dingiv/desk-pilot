//! Demo for `audio_aura_agent::client::AuraClient` — exercises BOTH planes against a running
//! aura-daemon:
//! - control plane: one-shot `GET /api/state` (settings snapshot);
//! - data plane:    `subscribe_events()` — live recognition events (5-event boundary paradigm)
//!   pushed directly.
//!
//! Run: `cargo run -p audio-aura-agent --example client_demo -- http://127.0.0.1:9091`
//! (Start aura-daemon first.)

use audio_aura_agent::client::AuraClient;
use audio_aura_agent::AsrEvent;
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

    // ── data plane: live recognition events (low-latency push) ──
    let events = client.subscribe_events();
    tokio::pin!(events); // the async_stream is !Unpin — pin before .next()
    while let Some(ev) = events.next().await {
        let t = chrono::Local::now().format("%H:%M:%S%.3f");
        match ev {
            AsrEvent::StreamFragment {
                paragraph_id,
                sentence_id,
                text,
                ..
            } => {
                println!("[{t}] stream    w{paragraph_id}/s{sentence_id} | {text}")
            }
            AsrEvent::BatchSentence {
                paragraph_id,
                sentence_id,
                text,
            } => {
                println!("[{t}] batch-sen w{paragraph_id}/s{sentence_id} | {text}")
            }
            AsrEvent::BatchParagraph { paragraph_id, text } => {
                println!("[{t}] batch-par w{paragraph_id} | {text}")
            }
            AsrEvent::SentenceCalibration {
                paragraph_id,
                calibrated,
            } => {
                println!("[{t}] sen-calib w{paragraph_id} | {calibrated}")
            }
            AsrEvent::ParagraphCalibration {
                paragraph_id,
                calibrated,
            } => {
                println!("[{t}] FINAL     w{paragraph_id} | {calibrated}")
            }
            AsrEvent::Correction {
                paragraph_id,
                raw,
                corrected,
            } => {
                println!("[{t}] CORRECT   w{paragraph_id} | {raw:?} → {corrected:?}");
            }
        }
    }
    Ok(())
}
