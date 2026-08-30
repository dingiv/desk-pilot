//! fsm — 输入组合状态机层。
//!
//! - [`state`] — `StateMachine`(ComposeState 组合状态机)+ `StepEnv` 注入接口
//! - [`router`] — `StateMachineTable` 键路由表:所有前端不再拦截任何键,
//!   每枚键都忠实传入,由路由表决定"输入法消费还是透传"
//!
//! 兼容别名:`lib.rs` 以 `pub use fsm::{router, state}` 保留旧路径
//! `ime_core::state` / `ime_core::router`(外部 apps 未迁移)。
pub mod router;
pub mod state;
