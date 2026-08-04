//! Bridge between the aura-agent SDK (`AuraClient`) and geek-familiar's iced app.
//!
//! The SDK uses `reqwest` + `tokio`; we spawn a dedicated tokio runtime on its own
//! thread. The iced subscription receives `AuraStateView` snapshots through an mpsc
//! channel, and command-like requests (toggle scout, correct, fetch audio) go through
//! the same tokio runtime via a oneshot/task pattern.

use std::sync::Arc;
use std::thread;

use audio_aura_agent::client::AuraClient;
use audio_aura_agent::view::AuraStateView;
use futures::StreamExt;
use tokio::runtime::Runtime;

/// Spawn a dedicated tokio runtime + AuraClient, piping `AuraStateView` snapshots
/// into the returned iced stream. This is the replacement for the old `asr_stream`.
pub fn aura_stream(
    base: &String,
) -> std::pin::Pin<Box<dyn iced::futures::Stream<Item = AuraStateView> + Send>> {
    let base = if base.starts_with("http") { base.clone() } else { format!("http://{base}") };
    Box::pin(iced::stream::channel::<AuraStateView>(16, move |mut tx: iced::futures::channel::mpsc::Sender<AuraStateView>| async move {
        let _jh = thread::spawn(move || {
            let rt = Runtime::new().expect("tokio runtime for aura-client");
            rt.block_on(async move {
                let client = match AuraClient::new(&base) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[aura-client] AuraClient::new({base}): {e}");
                        return;
                    }
                };
                // Fetch initial state immediately
                match client.state().await {
                    Ok(snap) => {
                        eprintln!("[aura-client] initial snapshot: {} utterances, connected={}", snap.utterances.len(), snap.connected);
                        let _ = tx.try_send(snap);
                    }
                    Err(e) => eprintln!("[aura-client] initial state fetch failed: {e}"),
                }
                // Subscribe to state changes
                let mut stream = Box::pin(client.subscribe(400));
                while let Some(snap) = stream.next().await {
                    if tx.try_send(snap).is_err() {
                        break; // receiver dropped
                    }
                }
            });
        });
        // Keep the iced stream alive forever
        std::future::pending::<()>().await;
    }))
}

/// All aura-daemon commands use this shared handle.
#[derive(Clone)]
pub struct AuraHandle {
    inner: Arc<AuraHandleInner>,
}

struct AuraHandleInner {
    rt: Runtime,
    client: AuraClient,
}

impl AuraHandle {
    /// Create a new handle (owns its own tokio runtime, one per app instance).
    pub fn new(addr: &str) -> Result<Self, String> {
        let base = if addr.starts_with("http") { addr.to_string() } else { format!("http://{addr}") };
        let rt = Runtime::new().map_err(|e| format!("tokio: {e}"))?;
        let client = AuraClient::new(&base).map_err(|e| format!("AuraClient({base}): {e}"))?;
        Ok(Self {
            inner: Arc::new(AuraHandleInner { rt, client }),
        })
    }

    /// `POST /api/control/scout {enabled}` — toggle scout recording.
    pub fn toggle_scout(&self, enabled: bool) {
        let h = self.inner.clone();
        thread::spawn(move || {
            let _ = h.rt.block_on(h.client.set_connected(enabled));
        });
    }

    /// `POST /api/correct {seq, raw, corrected}` — submit a correction.
    pub fn correct(&self, seq: u64, raw: &str, corrected: &str) {
        let h = self.inner.clone();
        let raw = raw.to_string();
        let corrected = corrected.to_string();
        thread::spawn(move || {
            let _ = h.rt.block_on(h.client.correct(seq, &raw, &corrected));
        });
    }

    /// `GET /api/audio/{seq}` — fetch WAV bytes, then play via pw-play.
    pub fn play_audio(&self, seq: u64) {
        let h = self.inner.clone();
        thread::spawn(move || {
            let result = h.rt.block_on(h.client.audio(seq));
            match result {
                Ok(bytes) if !bytes.is_empty() => {
                    let path = format!("/tmp/geek-familiar-audio-{seq}.wav");
                    if std::fs::write(&path, &bytes).is_err() { return; }
                    let _ = std::process::Command::new("pw-play").arg(&path).status();
                }
                Ok(_) => eprintln!("[aura-handle] audio {seq}: empty"),
                Err(e) => eprintln!("[aura-handle] audio {seq}: {e}"),
            }
        });
    }

    /// `GET /health` — check daemon reachability (fire-and-forget for now).
    pub fn health_check(&self) {
        let h = self.inner.clone();
        thread::spawn(move || {
            match h.rt.block_on(h.client.health()) {
                Ok(true) => eprintln!("[aura-handle] daemon reachable"),
                Ok(false) => eprintln!("[aura-handle] daemon NOT reachable"),
                Err(e) => eprintln!("[aura-handle] health check: {e}"),
            }
        });
    }
}
