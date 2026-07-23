//! Cross-subsystem communication (Phase 2).
#![allow(dead_code)]
//!
//! - **aura SSE client**: connects to aura-daemon (:9091), subscribes to `/api/stream`,
//!   accumulates `AsrBuffer` for `#asr` voice insertion.
//! - **familiar TCP server**: listens on :9601, accepts familiar connections for snippet
//!   config push + status display.
//!
//! Phase 1 (current): all stubs. The fcitx5 addon works standalone with static snippets.
//! Phase 2: spawn these as background threads in `swift_ime_init()`.

/// Placeholder: the most recent voice text from aura SSE (for `#asr` trigger).
pub struct AsrBuffer;

impl AsrBuffer {
    pub fn new() -> Self { AsrBuffer }
    /// The current calibrated text buffer (empty in Phase 1).
    pub fn snapshot(&self) -> String { String::new() }
}

/// Spawn the aura SSE client thread (Phase 2 stub).
pub fn spawn_aura_sse() {
    tracing::info!("aura SSE client — stub (Phase 2)");
}

/// Spawn the familiar TCP server thread (Phase 2 stub).
pub fn spawn_familiar_server() {
    tracing::info!("familiar TCP server :9601 — stub (Phase 2)");
}
