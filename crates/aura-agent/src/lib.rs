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

pub mod capability;
pub mod tool;

pub use capability::{
    ContextSummarizer, CorrectionSample, FineTuneHandle, FineTuner, HotwordManager,
    SharedHotwordManager, StubContextSummarizer, StubFineTuner, StubMemoryStore, MemoryStore,
};
pub use tool::{AddHotwordTool, Tool};
