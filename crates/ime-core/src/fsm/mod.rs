//! fsm — 输入组合状态机层,文件按三阶段组织(round9)。
//!
//! - [`state`] — `StateMachine`(原 `StateMachineTable`):stage1 入口,
//!   `.step(key)` 委托 [`pre::ControlStage`] 判定"输入法消费还是透传"
//! - [`family`] — `FamilyPipeline`(原 `StateMachine`):**stage2 家族分发**
//!   + 状态聚合体(comp/panel/magic)+ `StepEnv` 注入接口
//! - [`chain`] — `'` 链式输入的段解析(纯函数):`ti'an` / `X'#cmd` /
//!   `X''#cmd` 的语法在此,状态机与拼音家族按它路由
//!
//! - [`post`] — **stage3 后处理**:合成 → 全局调整 → 造词单字区 → 视图组装
pub mod chain;
pub mod family;
pub mod post;
pub mod pre;
pub mod state;
