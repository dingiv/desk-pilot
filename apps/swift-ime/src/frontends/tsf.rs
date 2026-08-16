//! Windows TSF COM text service backend (Phase 5 — stub).
#![allow(dead_code)]

use ime_core::ImeView;

pub struct TsfAdapter;

impl Default for TsfAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl TsfAdapter {
    pub fn new() -> Self { TsfAdapter }
}

impl super::PlatformAdapter for TsfAdapter {
    fn activate(&mut self) {}
    fn deactivate(&mut self) {}
    fn reset(&mut self) {}
    fn process_key(&mut self, _ch: char) -> ImeView { ImeView::empty() }
    fn select_candidate(&mut self, _index: usize) -> ImeView { ImeView::empty() }
}
