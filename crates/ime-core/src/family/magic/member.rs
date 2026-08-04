//! MagicMember — one magic command (a Member of the Magic family).
//!
//! `#asr`, `#req`, `#date` … are all members of [`MagicFamily`]. A **live** member
//! owns an interactive session: after its trigger completes, the FSM enters
//! [`ComposeState::Magic`] and routes keys + async ticks to the spawned member
//! instance. A **static** member never activates — it resolves to a fixed
//! expansion text inline.
//!
//! ## Adding a command
//! Implement [`MagicMember`] and register it in [`MagicFamily::new`] — matcher
//! entries, prediction hints and activation dispatch are all generated from the
//! registry. No engine / FSM special-casing needed.

use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};

/// Result of feeding one key to the active live member.
#[derive(Debug)]
pub enum MemberAction {
    /// The member consumed the key — show this view and stay active.
    View(ImeView),
    /// The member is done — commit this text and return to Idle.
    Commit(String),
    /// The member is done — exit without committing (cancel).
    Exit,
}

/// A single magic command.
pub trait MagicMember: Send + Sync {
    /// Command name, also the trigger suffix (e.g. "asr" → "#asr").
    fn name(&self) -> &'static str;

    /// Short description shown in the prediction hint.
    fn description(&self) -> &'static str;

    /// Matcher activation token (e.g. "__ASR_BUFFER__"). When the user completes
    /// the trigger, the FSM looks this token up in the registry and spawns a
    /// fresh instance. `None` = not a live command.
    fn activation_token(&self) -> Option<&'static str> {
        None
    }

    /// Static expansion text (e.g. `#date` → "2026-07-27"). `None` = live command.
    /// Static members are expanded inline by the snippet path, never activated.
    fn static_expansion(&self) -> Option<String> {
        None
    }

    /// Extra triggers that resolve to this same member (e.g. "#flush" → voice).
    fn aliases(&self) -> &[&'static str] {
        &[]
    }

    /// Fresh per-context instance. Each activation gets its own — a member holds
    /// per-session state (typed suffix, last-seen version, …); shared resources
    /// live behind `Arc`s.
    fn spawn(&self) -> Box<dyn MagicMember>;

    /// Enter the command: build the initial candidates / preedit into `sm`.
    fn activate(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> ImeView;

    /// One key while this member is active.
    fn on_key(&mut self, sm: &mut StateMachine, ch: char, env: &dyn StepEnv) -> MemberAction;

    /// Async refresh — called by the engine's tick loop (TUI render / fcitx5
    /// timer). Return `Some(view)` if the member rebuilt the candidate view.
    fn tick(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> Option<ImeView>;

    /// The member session ended (commit / cancel / reset). In-flight background
    /// work keeps running via shared `Arc`s — nothing to cancel by default.
    fn deactivate(&mut self) {}

    /// Full (un-truncated) texts of the current candidates, for display paths
    /// that want the commit-able text (TUI detailed view). Default: the previews
    /// currently in `sm.candidates`.
    fn candidate_texts(&self, sm: &StateMachine) -> Vec<String> {
        sm.candidates.clone()
    }
}

// ── Shared display helpers ───────────────────────────────────────────────

/// Max displayed bytes for a live candidate preview (≈20 CJK chars). Longer texts get
/// `"first…"` — the full text lives in the member's own state and is committed by Space,
/// so truncation here is cosmetic.
pub const CANDIDATE_PREVIEW_MAX: usize = 60;

/// First `max` bytes (char-boundary-safe, never splits a multi-byte char) + `…` if
/// truncated.
pub fn preview_text(text: &str, max: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
