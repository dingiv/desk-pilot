//! Platform-agnostic types. ImeView is the cross-platform UI state snapshot
//! returned by the engine after every key event. Platform frontends diff it
//! against the previous frame and apply only the changes.

/// Pinyin-to-hanzi engine. Community crate `inputx-pinyin` fills this.
pub trait PinyinEngine: Send + Sync {
    /// Given a pinyin string, return candidate hanzi strings (empty if no match).
    fn candidates(&self, pinyin: &str) -> Vec<String>;

    /// Extract the first valid pinyin syllable from the input.
    /// E.g., "lizhengming" → Some("li"), "kuifa" → Some("kui").
    fn first_syllable(&self, pinyin: &str) -> Option<String>;

    /// Record a user pick in inputx-pinyin's L0 layer for frequency boosting.
    /// After 3 picks the word auto-pins to the top.
    fn record_pick(&self, pinyin: &str, word: &str);

    /// Learn a new phrase — save it for future sessions (PhraseBook).
    fn learn_phrase(&self, pinyin: &str, hanzi: &str);
}

// ── ImeView: the cross-platform UI state snapshot ─────────────────────────

pub const CANDIDATE_SLOTS: usize = 16;

/// One candidate in the ImeView. Fixed-size for C ABI compatibility. `text` is 128 bytes so a
/// full voice sentence (~40+ CJK chars) fits; longer candidates truncate cleanly at a char
/// boundary (see [`ImeView::set_str`]).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CandidateSlot {
    pub text: [u8; 128],
    pub label: [u8; 8],
    /// 调试模式元数据:候选词的提供者与权重,如 `[0.960 pinyin/lattice]`。
    /// 仅 `candidate_meta_enabled` 时填充;前端(如 fcitx 的候选 comment)据此显示。
    pub meta: [u8; 32],
}

impl Default for CandidateSlot {
    fn default() -> Self {
        CandidateSlot { text: [0u8; 128], label: [0u8; 8], meta: [0u8; 32] }
    }
}

impl CandidateSlot {
    pub fn from_str(text: &str) -> Self {
        let mut s = CandidateSlot::default();
        ImeView::set_str(&mut s.text, text);
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
    /// Byte offset within `commit_text` where the application caret should sit
    /// after committing. Always ≤ committed length (a `$CURSOR` marker in a
    /// snippet template places it mid-text; every other commit leaves it at the
    /// end). Frontends move the caret accordingly — e.g. fcitx5 commits then
    /// forwards `Left` keys back by `len - commit_cursor`.
    pub commit_cursor: u32,
    /// Preedit slot: 512 bytes so a magic anchor can expand into the recognized
    /// text (e.g. `🎙 #asr 今天天气真不错…`) — long voice sentences fit whole.
    pub preedit_text: [u8; 512],
    pub preedit_cursor: u32,
    pub candidates: [CandidateSlot; CANDIDATE_SLOTS],
    pub candidate_count: u32,
    pub candidate_highlight: u32,
    pub candidate_page: u32,
    pub candidate_page_size: u32,
    /// Aux-up mirrors the preedit; same size so an expanded preedit is never cut here.
    pub aux_up: [u8; 512],
    pub key_passthrough: u8,
}

impl ImeView {
    /// Empty view — no commit, no preedit, no candidates, key not through.
    pub fn empty() -> Self {
        ImeView {
            commit_text: [0u8; 512],
            commit_cursor: 0,
            preedit_text: [0u8; 512],
            preedit_cursor: 0,
            candidates: [CandidateSlot::default(); CANDIDATE_SLOTS],
            candidate_count: 0,
            candidate_highlight: 0,
            candidate_page: 0,
            candidate_page_size: 7,
            aux_up: [0u8; 512],
            key_passthrough: 0,
        }
    }

    /// Fill a string field in the view (NUL-terminated). Truncates at the last **char boundary**
    /// ≤ `buf.len()-1` — never splits a multi-byte character, so the result is always valid UTF-8
    /// (byte-truncation would produce garbage that crashes fcitx5's UI with "Invalid utf8 string").
    pub fn set_str(buf: &mut [u8], s: &str) {
        buf.fill(0);
        let bytes = s.as_bytes();
        let max = buf.len().saturating_sub(1); // leave room for the NUL
        if bytes.len() <= max {
            buf[..bytes.len()].copy_from_slice(bytes);
            return;
        }
        // Walk back from `max` to the nearest char boundary — ensures the truncated string is
        // valid UTF-8 even when `max` falls in the middle of a multi-byte sequence.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        buf[..end].copy_from_slice(&bytes[..end]);
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
    use super::*;

    #[test]
    fn ime_state_defaults() {
        let sm = crate::state::StateMachine::new();
        assert!(sm.buffer.is_empty());
        assert_eq!(sm.state, crate::state::ComposeState::Idle);
    }

    #[test]
    fn set_str_truncates_at_char_boundary() {
        let mut buf = [0u8; 10]; // room for 9 chars + NUL
        // 9 ASCII bytes fits exactly (no truncation).
        ImeView::set_str(&mut buf, "123456789");
        assert_eq!(ImeView::str_field(&buf), "123456789");

        // 10-byte ASCII: truncated, but ASCII is 1-byte → any byte is a char boundary.
        let mut buf = [0u8; 10];
        ImeView::set_str(&mut buf, "1234567890"); // 10 bytes, max=9 → truncate
        assert_eq!(ImeView::str_field(&buf), "123456789"); // 9 chars

        // CJK: "你好世界" = 12 bytes. buf len=10 → max=9. Bytes 0-8 hold "你好世" (all 3 chars fit
        // on char boundaries). Byte 9 starts "界" → doesn't fit → truncates to 3 chars.
        let mut buf = [0u8; 10];
        ImeView::set_str(&mut buf, "你好世界"); // 你(0-2) 好(3-5) 世(6-8) 界(9-11)
        let result = ImeView::str_field(&buf);
        assert_eq!(result, "你好世", "3 full CJK chars (9 bytes) fit in 9-byte buf");
        assert!(!result.is_empty());

        // buf too small: "你好" = 6 bytes, buf len=5 → max=4 → walks back to 3 ("你") → "你".
        let mut buf = [0u8; 5];
        ImeView::set_str(&mut buf, "你好");
        assert_eq!(ImeView::str_field(&buf), "你");
    }
}
