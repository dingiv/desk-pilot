//! Data model types — configuration, state, and messages.
//!
//! Pure data; no business logic or rendering.

// ── Configuration (deserialized from familiar.yaml) ──────────────────────────

/// Which side of the desktop the pet docks to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockingPreference {
    #[default]
    Left,
    Right,
}

/// Theme colours loaded from `familiar.yaml` (or defaults).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StyleConfig {
    // ── card / panel ──
    #[serde(default = "def_card_bg")]
    pub card_bg: [f32; 4],
    #[serde(default = "def_shadow_color")]
    pub shadow_color: [f32; 4],
    #[serde(default)]
    pub shadow_blur: Option<f32>,
    #[serde(default)]
    pub shadow_offset: Option<[f32; 2]>,

    // ── pill buttons ──
    #[serde(default = "def_pill_accent")]
    pub pill_accent: [f32; 4],
    #[serde(default = "def_pill_neutral")]
    pub pill_neutral: [f32; 4],
    /// Accent gradient for pills: `[angle, stop0, stop1]` in degrees,
    /// e.g. `[135, 0.35, 0.55, 0.95, 1.0, 0.25, 0.45, 0.90, 0.95]`
    /// → angle=135°, stop@0=(0.35,0.55,0.95,1.0), stop@1=(...)
    #[serde(default)]
    pub pill_accent_gradient: Option<Vec<f32>>,
}
fn def_card_bg() -> [f32; 4] { [0.12, 0.12, 0.18, 0.92] }
fn def_shadow_color() -> [f32; 4] { [0.0, 0.0, 0.0, 0.35] }
fn def_pill_accent() -> [f32; 4] { [0.35, 0.55, 0.95, 0.95] }
fn def_pill_neutral() -> [f32; 4] { [0.20, 0.20, 0.26, 0.85] }

impl Default for StyleConfig {
    fn default() -> Self {
        Self {
            card_bg: def_card_bg(),
            shadow_color: def_shadow_color(),
            shadow_blur: None,
            shadow_offset: None,
            pill_accent: def_pill_accent(),
            pill_neutral: def_pill_neutral(),
            pill_accent_gradient: None,
        }
    }
}

// ── Tabs ─────────────────────────────────────────────────────────────────────

/// Which dock tab is currently open (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Chat,
    Settings,
}

// ── Messages ─────────────────────────────────────────────────────────────────

/// Every event the pet can respond to.
#[derive(Debug, Clone)]
pub enum Message {
    // ── audio-aura ──
    Asr(crate::service::asr::AsrUpdate),
    ToggleRecording,
    HandshakeDone(bool),
    HealthCheck(bool),
    #[allow(unused)]
    RecordingToggled,
    /// Clipboard content from GNOME extension push OR iced poll.
    ClipboardUpdate(String),
    /// Periodic poll fallback (no extension / native run).
    ClipboardPoll,

    // ── interaction ──
    DragStarted,
    TabPressed(Panel),
    ImeInput(String),
    ToggleAutoMove,

    // ── click-through ──
    ScreenshotReady(iced::window::screenshot::Screenshot),
    PassthroughApplied(usize),
    RescanTick,

    // ── resize ──
    DiagonalResizeStart,

    // ── window ──
    Quit,
}

// ── ASR state ────────────────────────────────────────────────────────────────

pub use crate::service::asr::ConversationTurn;

#[derive(Default)]
pub struct AsrState {
    pub connected: bool,
    pub interim: String,
    pub history: Vec<ConversationTurn>,
}
