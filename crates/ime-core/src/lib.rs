//! ime-core — the pure Rust engine behind swift-ime: trie-based snippet matching, text expansion
//! with variable substitution, a middleware dispatch chain, and a hot-reloadable snippet store.
//! Zero OS dependencies — cross-compilable and fully unit-testable.

pub mod asr_buffer;
pub mod dispatcher;
pub mod engine;
pub mod expander;
pub mod family;
pub mod matcher;
pub mod recency;
pub mod scoring;
pub mod special_key;
pub mod platform;
pub mod store;
pub mod snippet_store;
pub mod state;

pub use dispatcher::Dispatcher;
pub use expander::Expander;
pub use matcher::Matcher;
pub use platform::{ImeView, CandidateSlot, CANDIDATE_SLOTS, PinyinEngine};
pub use snippet_store::SnippetStore;
