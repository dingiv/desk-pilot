//! audio-aura-agent — async HTTP / SSE client SDK for the aura daemon.
//!
//! 仅保留 [`AuraClient`] + 共享数据类型。原先的 `AuraAgent` managed-state facade
//! 已被 ime-core 内部的 voice listener + `SharedVoiceState` 取代 ——
//! engine 构造时启动 voice listener 长期持有 `AuraClient`,在 IoThread 的
// tokio runtime 上 await SSE 数据面 + 健康探针。
//!
//! Stage3 capability trait / rule trigger 见 `capability.rs` / `rules.rs`(保留)。

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
pub use client::AuraClient;
pub use view::{AsrSegment, AuraStateView, ConfigView, CorrectionView, VadView};