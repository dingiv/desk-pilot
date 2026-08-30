//! ime-core — the pure Rust engine behind swift-ime: trie-based snippet matching, text expansion
//! with variable substitution, a middleware dispatch chain, and a hot-reloadable snippet store.
//! Zero OS dependencies — cross-compilable and fully unit-testable.

pub mod dispatcher;
pub mod engine;
pub mod family;
pub mod fsm;
pub mod frontend;
pub mod io_thread;
pub mod matcher;
pub mod scoring;
pub mod store;
pub mod voice_state;

// ── 兼容别名(旧路径;新代码请用规范路径)────────────────────────────
// expander → family::magic::expander(snippet/魔法命令的模板展开器,归位)
pub use family::magic::expander;
// 状态机层 → fsm(组合同步状态机 + 键路由表)
pub use fsm::router;
pub use fsm::state;

pub use dispatcher::Dispatcher;
pub use expander::Expander;
pub use matcher::Matcher;
pub use frontend::{CandidateSlot, ImeView, CANDIDATE_SLOTS};
pub use router::{KeyEvent, KeyKind, StateFlags, StateMachineTable};
