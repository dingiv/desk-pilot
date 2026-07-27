//! ime-core — the pure Rust engine behind swift-ime: trie-based snippet matching, text expansion
//! with variable substitution, a middleware dispatch chain, and a hot-reloadable snippet store.
//! Zero OS dependencies — cross-compilable and fully unit-testable.

pub mod dispatcher;
pub mod expander;
pub mod matcher;
pub mod pinyin;
pub mod platform;
pub mod snippet_store;
pub mod state;

pub use dispatcher::Dispatcher;
pub use expander::Expander;
pub use matcher::Matcher;
pub use platform::{ImeView, CandidateSlot, CANDIDATE_SLOTS, PinyinEngine};
pub use snippet_store::SnippetStore;
