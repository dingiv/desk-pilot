//! Special key handling layer — intercepted BEFORE character prediction.
//!
//! All frontends (fcitx5, TUI, ibus, …) forward non-character keys through
//! this layer so navigation, commit, and selection are handled consistently
//! by the engine, not duplicated in each adapter.
//!
//! ## Key bindings
//!
//! | Key | Action |
//! |-----|--------|
//! | ↑ ↓ | move highlight |
//! | ← → / Tab | move highlight |
//! | PgUp PgDown | change page |
//! | [ ] | move cursor in preedit |
//! | Space | commit highlighted candidate |
//! | Enter | force-commit raw input |
//! | Escape | reset |
//! | 1-9 | select candidate by index |
//! | Backspace | pop last character |
//! | + - | reserved |
//!
//! ## Return value
//!
//! `Some(ImeView)` means the key was handled and no further processing
//! is needed. `None` means the key is a regular character — pass it to
//! the normal prediction pipeline.

use crate::platform::ImeView;
use crate::state::StepEnv;

/// Special keys recognized by the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpecialKey {
    Up,
    Down,
    Left,
    Right,
    Tab,
    PageUp,
    PageDown,
    Space,
    Enter,
    Escape,
    Backspace,
    BracketLeft,
    BracketRight,
    Digit(u8), // 1-9
}

impl SpecialKey {
    /// Decode from a C ABI integer code.
    pub fn from_code(code: i32) -> Option<Self> {
        match code {
            1 => Some(SpecialKey::Up),
            2 => Some(SpecialKey::Down),
            3 => Some(SpecialKey::Left),
            4 => Some(SpecialKey::Right),
            5 => Some(SpecialKey::Tab),
            6 => Some(SpecialKey::PageUp),
            7 => Some(SpecialKey::PageDown),
            10 => Some(SpecialKey::Space),
            11 => Some(SpecialKey::Enter),
            12 => Some(SpecialKey::Escape),
            13 => Some(SpecialKey::Backspace),
            20 => Some(SpecialKey::BracketLeft),
            21 => Some(SpecialKey::BracketRight),
            d @ 101..=109 => Some(SpecialKey::Digit((d - 100) as u8)),
            _ => None,
        }
    }
}

/// Process a special key for the given state machine and context.
/// Returns `Some(view)` if handled, `None` if the key should be passed
/// to normal character prediction.
pub fn handle_special_key(
    sm: &mut crate::state::StateMachine,
    key: SpecialKey,
    env: &dyn StepEnv,
) -> Option<ImeView> {
    match key {
        SpecialKey::Up => {
            sm.move_highlight(-1);
            Some(sm.view(env))
        }
        SpecialKey::Down => {
            sm.move_highlight(1);
            Some(sm.view(env))
        }
        SpecialKey::Left | SpecialKey::Tab => {
            sm.move_highlight(-1);
            Some(sm.view(env))
        }
        SpecialKey::Right => {
            sm.move_highlight(1);
            Some(sm.view(env))
        }
        SpecialKey::PageUp => {
            sm.change_page(-1);
            Some(sm.view(env))
        }
        SpecialKey::PageDown => {
            sm.change_page(1);
            Some(sm.view(env))
        }
        SpecialKey::BracketLeft => {
            if sm.cursor > 0 {
                sm.cursor = sm.cursor.saturating_sub(1);
            }
            Some(sm.view(env))
        }
        SpecialKey::BracketRight => {
            let max = sm.preedit.chars().count();
            if sm.cursor < max {
                sm.cursor += 1;
            }
            Some(sm.view(env))
        }
        SpecialKey::Space => {
            // Space in any compose state: commit.
            let view = sm.step(' ', env);
            Some(view)
        }
        SpecialKey::Enter => {
            // Force-commit raw input.
            let view = sm.step('\n', env);
            Some(view)
        }
        SpecialKey::Escape => {
            sm.reset();
            Some(ImeView::empty())
        }
        SpecialKey::Backspace => {
            let view = sm.step('\x08', env);
            Some(view)
        }
        SpecialKey::Digit(n) => {
            let idx = (n - 1) as usize;
            if idx < sm.candidates.len() {
                let view = sm.select(idx, env);
                Some(view)
            } else {
                None // out of range → pass to character
            }
        }
    }
}

// ── StateMachine helper methods ───────────────────────────────────────

impl crate::state::StateMachine {
    /// Build an ImeView from current state without processing a key.
    fn view(&self, env: &dyn StepEnv) -> ImeView {
        let mut view = ImeView::empty();
        self.fill_view_for_env(&mut view, env);
        view
    }

    /// Fills `view` with current candidates and preedit.
    fn fill_view_for_env(&self, view: &mut ImeView, env: &dyn StepEnv) {
        ImeView::set_str(&mut view.preedit_text, &self.preedit);
        view.preedit_cursor = self.cursor as u32;
        let n = self.candidates.len().min(16);
        for i in 0..n {
            ImeView::set_str(&mut view.candidates[i].text, &self.candidates[i]);
        }
        view.candidate_count = n as u32;
        view.candidate_highlight = self.candidate_highlight as u32;
        view.candidate_page = self.candidate_page as u32;
        view.candidate_page_size = self.candidate_page_size as u32;

        // Aux: show preedit for debug.
        if !self.preedit.is_empty() && n == 0 {
            ImeView::set_str(&mut view.aux_up, &self.preedit);
        }
        let _ = env; // keep signature consistent
    }

    /// Change page by delta.
    pub fn change_page(&mut self, delta: i32) {
        let n = self.candidates.len();
        if n == 0 || self.candidate_page_size == 0 { return; }
        let total_pages = (n + self.candidate_page_size - 1) / self.candidate_page_size;
        if total_pages <= 1 { return; }
        let new_page = (self.candidate_page as i32 + delta)
            .clamp(0, total_pages as i32 - 1) as usize;
        if new_page != self.candidate_page {
            self.candidate_page = new_page;
            self.candidate_highlight = new_page * self.candidate_page_size;
        }
    }
}
