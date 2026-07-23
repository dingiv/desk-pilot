//! macOS IMK input controller backend (Phase 5 — stub).
#![allow(dead_code)]

use ime_core::ImeAction;

pub struct ImkAdapter;

impl ImkAdapter {
    pub fn new() -> Self { ImkAdapter }
}

impl super::PlatformAdapter for ImkAdapter {
    fn activate(&mut self) {}
    fn deactivate(&mut self) {}
    fn reset(&mut self) {}
    fn process_key(&mut self, _ch: char) -> ImeAction { ImeAction::PassThrough }
    fn select_candidate(&mut self, _index: usize) -> ImeAction { ImeAction::PassThrough }
}
