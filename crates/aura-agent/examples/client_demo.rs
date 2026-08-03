//! Smoke test for `audio_aura_agent::client::AuraClient` — connect to a running aura-daemon,
//! subscribe to its state, and print each snapshot as it changes.
//!
//! Run: `cargo run -p audio-aura-agent --example client_demo -- http://127.0.0.1:9091`
//! (Start aura-daemon first on that port.)

use audio_aura_agent::client::AuraClient;
use futures::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:9091".into());
    let client = AuraClient::new(&base)?;
    eprintln!("connecting to {base} (ping ≥400ms) …  Ctrl-C 结束");

    // One-shot: show the daemon is up + its config before streaming.
    if client.health().await.unwrap_or(false) {
        let s = client.state().await?;
        eprintln!(
            "connected={} asr={}({}) llm={}:{} merge_gap={}s hotwords={}",
            s.connected,
            s.config.asr_backend,
            s.config.asr_provider,
            s.config.llm_kind,
            s.config.model,
            s.config.vad.merge_gap,
            s.hotwords.len(),
        );
    } else {
        eprintln!("daemon not reachable at {base} yet — will retry via subscribe()");
    }

    let states = client.subscribe(400);
    tokio::pin!(states); // the async_stream is !Unpin — pin before .next()
    while let Some(snap) = states.next().await {
        let live = snap.utterances.iter().rev().find(|u| u.live);
        let live_text = live.and_then(|u| u.calibrated.clone().or_else(|| Some(u.partial.clone())));
        println!(
            "[{}] connected={} 句数={} | live: {}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            snap.connected,
            snap.utterances.len(),
            live_text.as_deref().unwrap_or("…"),
        );
    }
    Ok(())
}
