//! ibus DBus engine backend (Phase 4 — stub).

use ime_core::ImeAction;

pub struct IbusAdapter;

impl IbusAdapter {
    pub fn new() -> Self { IbusAdapter }
}

impl super::PlatformAdapter for IbusAdapter {
    fn activate(&mut self) {}
    fn deactivate(&mut self) {}
    fn reset(&mut self) {}
    fn process_key(&mut self, _ch: char) -> ImeAction { ImeAction::PassThrough }
    fn select_candidate(&mut self, _index: usize) -> ImeAction { ImeAction::PassThrough }
}
