//! audio-aura-agent — the **Stage3 capability layer**. Defines the abilities a Stage3 agent (or,
//! per our architecture, the desktop-pet secretary that schedules them) can invoke to maintain
//! Stage1/Stage2 state and the user's long-term model:
//!
//! - [`HotwordManager`] — add/remove/list correction hotwords (feeds back into Stage2 immediately;
//!   Stage1 ASR-layer hotwords are baked by sherpa at recognizer creation, see TODO in AddHotword).
//! - [`FineTuner`] — trigger dynamic fine-tuning (LoRA) from accumulated correction samples.
//! - [`ContextSummarizer`] — condense the rolling context window into a long-term summary.
//! - [`MemoryStore`] — long-term key/value recall across sessions.
//!
//! **This crate holds CAPABILITIES only — no scheduling.** "When to fine-tune / which samples /
//! which hotword to add" is a decision for the secretary agent (desktop-pet), which calls these
//! capabilities over the daemon's socket. For the closed-loop demo, the daemon wires a simple
//! in-process rule trigger; desktop-pet replaces it later.
//!
//! This round implements only [`HotwordManager`] (+ [`SharedHotwordManager`]) and the
//! [`AddHotwordTool`]; the other capability traits are defined but stubbed.

pub mod agent;
pub mod capability;
pub mod client;
pub mod rules;
pub mod tool;
pub mod view;

pub use capability::{
    ContextSummarizer, CorrectionSample, FineTuneHandle, FineTuner, HotwordManager,
    SharedHotwordManager, StubContextSummarizer, StubFineTuner, StubMemoryStore, MemoryStore,
};
pub use tool::{AddHotwordTool, Tool};

// ── Stage3 规则触发器(闭环演示占位;desktop-pet 调度器接管)──────────────────────────
pub use rules::{looks_like_concat, stage3_rule_trigger};

// ── daemon↔client wire contract + async HTTP/SSE client SDK ──────────────────────────────
// Light on purpose (no mistralrs/asr): upper layers (desktop-pet, visual-rover, …) depend on
// THIS crate to talk to the aura-daemon without pulling the GPU inference stack.
pub use agent::{AgentEvent, AuraAgent, AuraConn, WindowView};
pub use client::AuraClient;
pub use view::{AsrSegment, AuraStateView, ConfigView, CorrectionView, VadView};
