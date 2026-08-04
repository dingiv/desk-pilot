//! Bridge between the aura-agent SDK (`AuraAgent`) and geek-familiar's iced app.
//!
//! The agent owns ALL interaction details — the connection, the background driver (its own tokio
//! runtime on a std thread), the control-plane snapshot, the data-plane utterance list, and
//! connectivity probing. geek-familiar only imports the crate, connects once, and reads
//! [`AgentEvent`]s from the iced subscription. No connection or state-management code of its own.

use std::sync::Arc;

use audio_aura_agent::agent::{AgentEvent, AuraAgent};
use futures::StreamExt;

/// Connect once, get a shared handle. The driver thread keeps state fresh in the background;
/// commands (`set_connected`, `correct`, `audio`) go through the same handle. Fire-and-forget.
pub fn connect(base: &str) -> Result<Arc<AuraAgent>, String> {
    let base = if base.starts_with("http") { base.to_string() } else { format!("http://{base}") };
    AuraAgent::connect(&base).map(Arc::new).map_err(|e| format!("AuraAgent({base}): {e}"))
}

/// iced subscription: forward the agent's event stream into the app's message channel.
/// iced's `Subscription::run_with` wants a fn pointer over the (Hash) id — the agent clone is
/// reconstructed from the `&Arc` on each re-run.
pub fn aura_stream(agent: &Arc<AuraAgent>) -> impl iced::futures::Stream<Item = AgentEvent> + Send {
    let agent = agent.clone();
    iced::stream::channel::<AgentEvent>(
        16,
        move |mut tx: iced::futures::channel::mpsc::Sender<AgentEvent>| async move {
            let mut events = Box::pin(agent.events()); // the SDK's stream is !Unpin — pin first
            while let Some(ev) = events.next().await {
                if tx.try_send(ev).is_err() {
                    break; // receiver dropped
                }
            }
            // The agent's stream is infinite (reconnects internally) — reaching here means the
            // subscription closed; keep the channel future alive either way.
            std::future::pending::<()>().await;
        },
    )
}

/// `GET /api/audio/{seq}` → write WAV to /tmp, play via pw-play. Runs off-thread (blocking).
pub fn play_audio(agent: Arc<AuraAgent>, seq: u64) {
    std::thread::spawn(move || {
        match agent.audio(seq) {
            Ok(bytes) if !bytes.is_empty() => {
                let path = format!("/tmp/geek-familiar-audio-{seq}.wav");
                if std::fs::write(&path, &bytes).is_err() {
                    return;
                }
                let _ = std::process::Command::new("pw-play").arg(&path).status();
            }
            Ok(_) => eprintln!("[geek-familiar] audio {seq}: empty"),
            Err(e) => eprintln!("[geek-familiar] audio {seq}: {e}"),
        }
    });
}
