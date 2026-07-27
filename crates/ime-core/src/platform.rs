//! Platform-agnostic types. ImeView is the cross-platform UI state snapshot
//! returned by the engine after every key event. Platform frontends diff it
//! against the previous frame and apply only the changes.

/// Pinyin-to-hanzi engine. Community crate `inputx-pinyin` fills this.
pub trait PinyinEngine: Send + Sync {
    /// Given a pinyin string, return candidate hanzi strings (empty if no match).
    fn candidates(&self, pinyin: &str) -> Vec<String>;
}

// ── ImeView: the cross-platform UI state snapshot ─────────────────────────

pub const CANDIDATE_SLOTS: usize = 16;

/// One candidate in the ImeView. Fixed-size for C ABI compatibility.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CandidateSlot {
    pub text: [u8; 64],
    pub label: [u8; 8],
}

impl Default for CandidateSlot {
    fn default() -> Self { CandidateSlot { text: [0u8; 64], label: [0u8; 8] } }
}

impl CandidateSlot {
    pub fn from_str(text: &str) -> Self {
        let mut s = CandidateSlot::default();
        let bytes = text.as_bytes();
        let n = bytes.len().min(s.text.len() - 1);
        s.text[..n].copy_from_slice(&bytes[..n]);
        s
    }
}

/// Complete UI state snapshot produced by the engine after processing one key
/// event. The platform frontend diffs this against the previous frame and
/// applies only the changes — like React's virtual DOM reconciliation.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ImeView {
    pub commit_text: [u8; 512],
    pub preedit_text: [u8; 256],
    pub preedit_cursor: u32,
    pub candidates: [CandidateSlot; CANDIDATE_SLOTS],
    pub candidate_count: u32,
    pub candidate_highlight: u32,
    pub candidate_page: u32,
    pub candidate_page_size: u32,
    pub aux_up: [u8; 256],
    pub key_passthrough: u8,
}

impl ImeView {
    /// Empty view — no commit, no preedit, no candidates, key not through.
    pub fn empty() -> Self {
        ImeView {
            commit_text: [0u8; 512],
            preedit_text: [0u8; 256],
            preedit_cursor: 0,
            candidates: [CandidateSlot::default(); CANDIDATE_SLOTS],
            candidate_count: 0,
            candidate_highlight: 0,
            candidate_page: 0,
            candidate_page_size: 7,
            aux_up: [0u8; 256],
            key_passthrough: 0,
        }
    }

    /// Fill a string field in the view (NUL-terminated).
    pub fn set_str(buf: &mut [u8], s: &str) {
        buf.fill(0);
        let bytes = s.as_bytes();
        let n = bytes.len().min(buf.len() - 1);
        buf[..n].copy_from_slice(&bytes[..n]);
    }

    pub fn str_field(buf: &[u8]) -> &str {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        std::str::from_utf8(&buf[..end]).unwrap_or("")
    }
}

impl Default for ImeView {
    fn default() -> Self {
        ImeView::empty()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ime_state_defaults() {
        let sm = crate::state::StateMachine::new();
        assert!(sm.buffer.is_empty());
        assert_eq!(sm.state, crate::state::ComposeState::Idle);
    }
}
