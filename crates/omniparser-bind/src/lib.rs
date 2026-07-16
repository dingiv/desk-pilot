//! napi-rs bindings for VRover's native driver layer.
//!
//! - [`OmniParserNative`] — YOLO icon detection + SoM annotation (wraps `vrover-omniparser`).
//! - [`DesktopNativeLayer`] — mouse + keyboard input via Linux uinput (wraps `vrover-drivers`).

use std::cell::RefCell;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use vrover_omniparser::{ElementType, OmniParser, OmniParserConfig};

// ── config ──────────────────────────────────────────────────────────────────

#[napi(object)]
pub struct OmniParserConfigNative {
    /// Path to the `icon_detect.onnx` weight file.
    pub yolo_path: String,
    /// Detection confidence threshold (default 0.05).
    pub box_threshold: f64,
    /// NMS IoU threshold (default 0.1).
    pub iou_threshold: f64,
}

// ── element ─────────────────────────────────────────────────────────────────

#[napi(object)]
pub struct ParsedElementNative {
    /// 0-based mark index (maps to SoM mark number via `mark + 1` on the TS side).
    pub mark: i32,
    /// `"Icon"` or `"Text"`.
    pub r#type: String,
    /// Bounding box left edge (pixels).
    pub x1: f64,
    /// Bounding box top edge (pixels).
    pub y1: f64,
    /// Bounding box right edge (pixels).
    pub x2: f64,
    /// Bounding box bottom edge (pixels).
    pub y2: f64,
    /// Whether the element is interactive (always `true` in Phase 1).
    pub interactivity: bool,
    /// Caption / OCR text, if available (Phase 2+).
    pub content: Option<String>,
}

// ── result ──────────────────────────────────────────────────────────────────

#[napi(object)]
pub struct ParseResult {
    /// PNG-encoded SoM-annotated image (same dimensions as input screenshot).
    pub annotated_png: Buffer,
    /// Detected elements with their bounding boxes.
    pub elements: Vec<ParsedElementNative>,
    /// Image width in pixels (from the decoded input PNG).
    pub width: u32,
    /// Image height in pixels (from the decoded input PNG).
    pub height: u32,
}

// ── parser ──────────────────────────────────────────────────────────────────

#[napi]
pub struct OmniParserNative {
    inner: RefCell<OmniParser>,
}

#[napi]
impl OmniParserNative {
    /// Create a new parser, loading the ONNX model from `config.yolo_path`.
    #[napi(constructor)]
    pub fn new(config: OmniParserConfigNative) -> Result<Self> {
        let mut cfg = OmniParserConfig::new(&config.yolo_path);
        cfg.box_threshold = config.box_threshold as f32;
        cfg.iou_threshold = config.iou_threshold as f32;

        let parser = OmniParser::new(cfg).map_err(|e| {
            napi::Error::from_reason(format!("OmniParser init failed: {e}"))
        })?;
        Ok(Self {
            inner: RefCell::new(parser),
        })
    }

    /// Parse one screenshot (PNG-encoded bytes).
    ///
    /// Decodes the PNG, runs YOLO icon detection + overlap removal + SoM
    /// annotation, and returns the annotated image + element list.
    #[napi]
    pub fn parse(&self, png_buffer: Buffer) -> Result<ParseResult> {
        // Decode PNG → RgbImage
        let img = image::load_from_memory(&png_buffer)
            .map_err(|e| napi::Error::from_reason(format!("PNG decode failed: {e}")))?
            .to_rgb8();
        let (w, h) = img.dimensions();

        // Run the full OmniParser pipeline
        let mut parser = self.inner.borrow_mut();
        let result = parser.parse(&img).map_err(|e| {
            napi::Error::from_reason(format!("OmniParser parse failed: {e}"))
        })?;

        // Convert elements to napi-friendly shape
        let elements: Vec<ParsedElementNative> = result
            .elements
            .into_iter()
            .map(|e| {
                let type_str = match e.r#type {
                    ElementType::Icon => "Icon".to_string(),
                    ElementType::Text => "Text".to_string(),
                };
                ParsedElementNative {
                    mark: e.mark as i32,
                    r#type: type_str,
                    x1: e.bbox.x1 as f64,
                    y1: e.bbox.y1 as f64,
                    x2: e.bbox.x2 as f64,
                    y2: e.bbox.y2 as f64,
                    interactivity: e.interactivity,
                    content: e.content,
                }
            })
            .collect();

        Ok(ParseResult {
            annotated_png: Buffer::from(result.annotated_png),
            elements,
            width: w,
            height: h,
        })
    }
}

// ══════════════════════════════════════════════════════════════════════════════
//  DesktopNativeLayer — uinput mouse + keyboard injection
// ══════════════════════════════════════════════════════════════════════════════

use vrover_drivers::{Button, InputSink, Key, UinputSink};

/// Mouse + keyboard input via Linux uinput (`/dev/uinput`).
///
/// Screen size is optional — when set, absolute pointer coords are scaled
/// correctly.  Requires write access to `/dev/uinput` (root or `uinput` group).
#[napi]
pub struct DesktopNativeLayer {
    sink: RefCell<UinputSink>,
}

#[napi]
impl DesktopNativeLayer {
    /// Open the uinput virtual device. Pass `(width, height)` for accurate
    /// pointer scaling; omit to use raw device-space coords.
    ///
    /// Requires `/dev/uinput` write access (root or `uinput` group) at runtime.
    #[napi(constructor)]
    pub fn new(screen_width: Option<u32>, screen_height: Option<u32>) -> Result<Self> {
        let sink = match (screen_width, screen_height) {
            (Some(w), Some(h)) => UinputSink::with_screen(w, h)
                .map_err(|e| napi::Error::from_reason(format!("uinput open failed: {e}")))?,
            _ => UinputSink::new()
                .map_err(|e| napi::Error::from_reason(format!("uinput open failed: {e}")))?,
        };
        Ok(Self { sink: RefCell::new(sink) })
    }

    #[napi]
    pub fn move_to(&self, x: i32, y: i32) -> Result<()> {
        self.sink.borrow_mut().move_to(x, y)
            .map_err(|e| napi::Error::from_reason(format!("uinput: {e}")))
    }

    #[napi]
    pub fn click(&self, x: i32, y: i32, button: String) -> Result<()> {
        let btn = parse_button(&button)?;
        self.sink.borrow_mut().click(x, y, btn)
            .map_err(|e| napi::Error::from_reason(format!("uinput: {e}")))
    }

    #[napi]
    pub fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        self.sink.borrow_mut().scroll(x, y, dx, dy)
            .map_err(|e| napi::Error::from_reason(format!("uinput: {e}")))
    }

    #[napi]
    pub fn type_text(&self, text: String) -> Result<()> {
        self.sink.borrow_mut().type_text(&text)
            .map_err(|e| napi::Error::from_reason(format!("uinput: {e}")))
    }

    #[napi]
    pub fn key_press(&self, key: String) -> Result<()> {
        let k = parse_key(&key)?;
        self.sink.borrow_mut().key_press(k)
            .map_err(|e| napi::Error::from_reason(format!("uinput: {e}")))
    }

    #[napi]
    pub fn key_release(&self, key: String) -> Result<()> {
        let k = parse_key(&key)?;
        self.sink.borrow_mut().key_release(k)
            .map_err(|e| napi::Error::from_reason(format!("uinput: {e}")))
    }

    #[napi]
    pub fn tap_key(&self, key: String) -> Result<()> {
        let k = parse_key(&key)?;
        self.sink.borrow_mut().tap_key(k)
            .map_err(|e| napi::Error::from_reason(format!("uinput: {e}")))
    }
}

// ── key / button parsing ────────────────────────────────────────────────────

fn parse_button(s: &str) -> Result<Button> {
    match s {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        other => {
            let n: u8 = other.parse().map_err(|_| {
                napi::Error::from_reason(format!("unknown button {other:?} — use left/right/middle"))
            })?;
            Ok(Button::Other(n))
        }
    }
}

fn parse_key(s: &str) -> Result<Key> {
    match s {
        "enter" | "Enter" => Ok(Key::Enter),
        "backspace" | "Backspace" => Ok(Key::Backspace),
        "tab" | "Tab" => Ok(Key::Tab),
        "escape" | "Escape" | "esc" => Ok(Key::Escape),
        "space" | "Space" => Ok(Key::Space),
        "delete" | "Delete" | "del" => Ok(Key::Delete),
        "insert" | "Insert" | "ins" => Ok(Key::Insert),
        "left" | "Left" => Ok(Key::Left),
        "right" | "Right" => Ok(Key::Right),
        "up" | "Up" => Ok(Key::Up),
        "down" | "Down" => Ok(Key::Down),
        "home" | "Home" => Ok(Key::Home),
        "end" | "End" => Ok(Key::End),
        "pageup" | "PageUp" => Ok(Key::PageUp),
        "pagedown" | "PageDown" => Ok(Key::PageDown),
        "leftshift" | "LeftShift" => Ok(Key::LeftShift),
        "rightshift" | "RightShift" => Ok(Key::RightShift),
        "leftcontrol" | "LeftControl" | "ctrl" => Ok(Key::LeftControl),
        "rightcontrol" | "RightControl" => Ok(Key::RightControl),
        "leftalt" | "LeftAlt" | "alt" => Ok(Key::LeftAlt),
        "rightalt" | "RightAlt" => Ok(Key::RightAlt),
        "leftsuper" | "LeftSuper" | "super" | "win" => Ok(Key::LeftSuper),
        "rightsuper" | "RightSuper" => Ok(Key::RightSuper),
        other => {
            // F1..F12
            if let Some(n) = other
                .strip_prefix('f')
                .or_else(|| other.strip_prefix('F'))
                .and_then(|n| n.parse::<u8>().ok())
            {
                if (1..=12).contains(&n) {
                    return Ok(Key::F(n));
                }
            }
            // Single char
            let mut chars = other.chars();
            if let Some(c) = chars.next() {
                if chars.next().is_none() {
                    return Ok(Key::Char(c));
                }
            }
            Err(napi::Error::from_reason(format!(
                "unknown key {other:?} — use names like enter/backspace/tab/escape/F1/a"
            )))
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
//  DesktopCapture — PipeWire ScreenCast capture (behind the `capture` feature)
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(feature = "capture")]
use std::sync::Mutex;
#[cfg(feature = "capture")]
use std::time::{Duration, Instant};
#[cfg(feature = "capture")]
use vrover_drivers::backends::pipewire::PipeWireSource;
#[cfg(feature = "capture")]
use vrover_drivers::CaptureSource;

/// Screen capture via PipeWire ScreenCast (the dialog-free GNOME/Wayland path).
///
/// The ScreenCast session is negotiated **once** at construction; the PipeWire
/// worker then runs on its own thread, and [`DesktopCapture::capture_screen`]
/// hands back the latest decoded frame. Requires a real graphical session +
/// xdg-desktop-portal at runtime (fails on a headless box).
#[cfg(feature = "capture")]
#[napi]
pub struct DesktopCapture {
    src: Mutex<PipeWireSource>,
}

#[cfg(feature = "capture")]
#[napi]
impl DesktopCapture {
    /// Negotiate the ScreenCast session and start the PipeWire worker. This
    /// talks to xdg-desktop-portal, so it needs a live graphical session.
    #[napi(constructor)]
    pub fn new() -> Result<Self> {
        let src = PipeWireSource::new().map_err(|e| {
            napi::Error::from_reason(format!("PipeWire capture init failed: {e}"))
        })?;
        Ok(Self {
            src: Mutex::new(src),
        })
    }

    /// Capture one frame: poll the PipeWire worker until a frame is ready
    /// (or `timeout_ms` elapses, default 30 000 ms) and return it PNG-encoded.
    #[napi]
    pub fn capture_screen(&self, timeout_ms: Option<u32>) -> Result<Buffer> {
        let timeout_ms = timeout_ms.unwrap_or(30_000);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            {
                let mut src = self.src.lock().expect("capture mutex poisoned");
                match src.capture() {
                    Ok(frame) => return Ok(Buffer::from(frame.to_png())),
                    // Worker still warming up (no first buffer yet) — retry.
                    Err(_) => {}
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Err(napi::Error::from_reason(format!(
            "PipeWire capture timed out after {timeout_ms}ms (no frame from worker)"
        )))
    }
}
