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
    /// Default body text colour [r, g, b, a].
    #[serde(default = "def_text_color")]
    pub text_color: [f32; 4],
    /// Secondary / label text. Defaults to `text_color`.
    #[serde(default = "def_text_color")]
    pub text_dim: [f32; 4],
    /// Placeholder / empty-state text. Defaults to `text_color`.
    #[serde(default = "def_text_color")]
    pub text_faint: [f32; 4],
    /// Subtle text (eg. reply, finer print). Defaults to `text_color`.
    #[serde(default = "def_text_color")]
    pub text_subtle: [f32; 4],
}
fn def_card_bg() -> [f32; 4] { [0.12, 0.12, 0.18, 0.92] }
fn def_shadow_color() -> [f32; 4] { [0.0, 0.0, 0.0, 0.35] }
fn def_pill_accent() -> [f32; 4] { [0.35, 0.55, 0.95, 0.95] }
fn def_pill_neutral() -> [f32; 4] { [0.20, 0.20, 0.26, 0.85] }
fn def_text_color() -> [f32; 4] { [0.90, 0.90, 0.95, 1.0] }

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
            text_color: def_text_color(),
            text_dim: def_text_color(),
            text_faint: def_text_color(),
            text_subtle: def_text_color(),
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
    // ── audio-aura (AuraAgent events — control plane + data plane in one stream) ──
    #[allow(unused)]
    AuraEvent(audio_aura_agent::agent::AgentEvent),
    ToggleRecording,
    HandshakeDone(bool),
    /// Trigger an aura health check (button press).
    CheckHealth,
    /// Result of a health check.
    HealthCheck(bool),
    #[allow(unused)]
    RecordingToggled,
    /// Clipboard content from GNOME extension push OR iced poll.
    ClipboardUpdate(String),
    /// Periodic poll fallback (no extension / native run).
    ClipboardPoll,
    /// User editing the scratchpad buffer (multi-line text_editor).
    ScratchpadEdit(iced::widget::text_editor::Action),
    /// Transcript buffer actions (read-only feel — we just let it bubble).
    TranscriptAction(iced::widget::text_editor::Action),

    // ── interaction ──
    DragStarted,
    TabPressed(Panel),
    /// iced_aw TabBar selection (maps to TabPressed + toggle).
    TabSelected(Panel),
    ImeInput(String),
    /// Right-click on an ASR entry → context menu (index into history).
    AsrContextMenu(u64),
    /// [✏ fix] — edit + submit correction for this turn (Step 3).
    FixTurn(u64),
    /// [🔊] — play TTS audio for this turn (Step 4).
    PlayAudio(u64),
    /// Keystroke in the inline correction text_input.
    CorrectionEdit(String),
    /// Submit the correction for this turn to aura.
    SubmitCorrection(u64),
    /// Cancel inline editing without submitting.
    CancelEdit,
    /// Audio playback result for a turn.
    AudioPlayed(u64, bool),
    /// App-level status message (errors, hints, info).
    AppStatus(String),
    /// Toggle a message in / out of the selection set.
    ToggleSelectTurn(u64),
    /// Copy all selected turns to clipboard.
    CopySelectedTurns,
    /// Toggle section collapse (0=ASR, 1=Clipboard, 2=Status).
    ToggleSection(usize),
    /// Start dragging the divider below section `idx`.
    SectionDragStart(usize),
    /// Mouse moved during section resize (carries Y in physical pixels).
    SectionDragMove(f32),
    /// End section divider drag.
    SectionDragEnd,
    /// A file was dropped onto the pet window from a file manager.
    FileDropped(String),
    /// File drag hovered over the pet window (visual feedback).
    FileHovered,
    /// File drag left the pet window (clear visual feedback).
    FileHoverLeft,
    /// Take a screenshot (area select, saves to /tmp/).
    TakeScreenshot,
    /// Screenshot saved to this path.
    ScreenshotSaved(String),
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

/// One conversation turn (synced from aura's `WindowView` — a settled window's calibrated text).
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    pub window_id: u64,
    pub user_text: String,
}

#[derive(Default)]
pub struct AsrState {
    /// SSE stream connected to aura daemon (transport layer).
    pub sse_connected: bool,
    /// Scout is actively recording (business layer).
    pub scout_active: bool,
    pub interim: String,
    pub history: Vec<ConversationTurn>,
}

impl AsrState {
    /// Three-way status for the ASR dock button.
    pub fn status(&self) -> AsrStatus {
        if !self.sse_connected {
            AsrStatus::Disconnected
        } else if self.scout_active {
            AsrStatus::Enabled
        } else {
            AsrStatus::Disabled
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrStatus {
    /// Not connected to aura daemon at all.
    Disconnected,
    /// Connected + scout active = recording live.
    Enabled,
    /// Connected but scout disabled (or failed to enable).
    Disabled,
}
