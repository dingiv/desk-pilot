//! ibus DBus engine backend (Phase 4 — stub).

use ime_core::ImeView;

pub struct IbusAdapter;

impl Default for IbusAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl IbusAdapter {
    pub fn new() -> Self { IbusAdapter }
}

impl super::PlatformAdapter for IbusAdapter {
    fn activate(&mut self) {}
    fn deactivate(&mut self) {}
    fn reset(&mut self) {}
    fn process_key(&mut self, _ch: char) -> ImeView { ImeView::empty() }
    fn select_candidate(&mut self, _index: usize) -> ImeView { ImeView::empty() }
}
