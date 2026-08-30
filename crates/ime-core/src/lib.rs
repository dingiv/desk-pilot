//! ime-core — the pure Rust engine behind swift-ime: trie-based snippet matching, text expansion
//! with variable substitution, a middleware dispatch chain, and a hot-reloadable snippet store.
//! Zero OS dependencies — cross-compilable and fully unit-testable.

pub mod chain;
pub mod dispatcher;
pub mod engine;
pub mod expander;
pub mod family;
pub mod frontend;
pub mod io_thread;
pub mod matcher;
pub mod platform;
pub mod recency;
pub mod router;
pub mod scoring;
pub mod snippet_store;
pub mod state;
pub mod store;
pub mod voice_state;

pub use dispatcher::Dispatcher;
pub use expander::Expander;
pub use matcher::Matcher;
pub use platform::{CandidateSlot, ImeView, CANDIDATE_SLOTS};
pub use router::{KeyEvent, KeyKind, StateFlags, StateMachineTable};
pub use snippet_store::SnippetStore;
