//! View layer — widget constructors, dock buttons, and tab panels.

pub mod chat;
pub mod dock;
pub mod settings;
pub mod style;

// Re-export public API
pub use chat::chat_panel;
pub use dock::{asr_dock_button, drag_button};
pub use settings::settings_panel;
pub use style::{card_style, parse_bg, pill_style, text_color, text_dim, text_faint, text_subtle};
