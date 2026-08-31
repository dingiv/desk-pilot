//! ime-core — the pure Rust engine behind swift-ime: trie-based snippet matching, text expansion
//! with variable substitution, a middleware dispatch chain, and a hot-reloadable snippet store.
//! Zero OS dependencies — cross-compilable and fully unit-testable.


pub mod engine;
pub mod family;
pub mod fsm;
pub mod frontend;
pub mod io_thread;

pub mod store;
pub mod voice_state;

// ── 兼容别名(旧路径;新代码请用规范路径)────────────────────────────
// expander → family::magic::expander(snippet/魔法命令的模板展开器,归位)
pub use family::magic::expander;
// (fsm::family 的 FamilyPipeline/StepEnv 经 ime_core::fsm::family 全路径使用)
// fsm 的类型经全路径使用(ime_core::fsm::state::… / ime_core::fsm::family::…)。
// 注意:顶层 `family` 名已被家族模块占用,fsm::family 不做顶层别名。


pub use expander::Expander;

pub use frontend::{CandidateSlot, ImeView, CANDIDATE_SLOTS};
pub use fsm::state::{KeyEvent, KeyKind, StateFlags, StateMachine};
