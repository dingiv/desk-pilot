//! Platform-agnostic traits and types. Each OS IME backend (fcitx5, ibus, TSF, IMK) implements
//! [`PlatformIme`]; the pure core never imports an OS API.

/// What the engine decides to do with a key event.
#[derive(Debug, Clone, PartialEq)]
pub enum ImeAction {
    /// Let the key pass through to the application unchanged.
    PassThrough,
    /// Show composed (preedit) text inline — not yet committed.
    Preedit { text: String, cursor: usize },
    /// Commit final text, replacing any current preedit.
    Commit(String),
    /// Show a candidate list for the user to choose from.
    Candidates {
        items: Vec<Candidate>,
        /// Index of the highlighted candidate (0-based).
        selected: usize,
    },
}

/// One entry in a candidate list.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The committed text if this candidate is selected.
    pub text: String,
    /// Short label shown in the candidate window (e.g. "snippet", "pinyin", "asr").
    pub label: String,
    /// Preview text shown next to the candidate (truncated expansion or first few hanzi).
    pub preview: String,
}

/// Mutable engine state that lives for the duration of this input session (one
/// `InputContext` in fcitx5 terms). Reset on focus change / Escape.
#[derive(Debug, Clone, Default)]
pub struct ImeState {
    /// Accumulated key characters since the last commit / reset.
    pub buffer: String,
    /// Which path the dispatcher is currently in.
    pub mode: InputMode,
    /// Cached hanzi candidates while in Pinyin mode (backs the candidate window +
    /// `select_candidate`). Empty outside Pinyin mode.
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// No trigger prefix seen yet — check all paths.
    #[default]
    Normal,
    /// Inside a snippet trigger (accumulating "/greet", "#asr", etc.).
    Trigger,
    /// Inside a pinyin sequence — showing hanzi candidates.
    Pinyin,
}

/// Pinyin-to-hanzi engine (Phase 3 — reserved trait, empty impl for Phase 1).
/// Community crate `inputx-pinyin` fills this later.
pub trait PinyinEngine: Send + Sync {
    /// Given a pinyin string, return candidate hanzi strings (empty if no match).
    fn candidates(&self, pinyin: &str) -> Vec<String>;
}

/// A no-op pinyin engine for Phase 1 (no Chinese input yet).
pub struct NoopPinyin;
impl PinyinEngine for NoopPinyin {
    fn candidates(&self, _pinyin: &str) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_pinyin_returns_empty() {
        let engine = NoopPinyin;
        assert!(engine.candidates("ni").is_empty());
    }

    #[test]
    fn ime_state_defaults() {
        let state = ImeState::default();
        assert!(state.buffer.is_empty());
        assert_eq!(state.mode, InputMode::Normal);
    }
}
