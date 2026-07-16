//! Shared types for the OmniParser port.

/// An axis-aligned box in **pixel** xyxy coordinates on the source image.
///
/// Phase 1 works in pixel space throughout (the Python normalizes to [0,1] ratios
/// then back; we skip the round-trip and carry pixels, which is what the SoM
/// renderer and the tool executor both want).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl BBox {
    #[inline]
    pub fn area(&self) -> f32 {
        (self.x2 - self.x1).max(0.0) * (self.y2 - self.y1).max(0.0)
    }

    /// Construct from center-form `(cx, cy, w, h)`.
    #[inline]
    pub fn from_cxcywh(cx: f32, cy: f32, w: f32, h: f32) -> Self {
        Self {
            x1: cx - w / 2.0,
            y1: cy - h / 2.0,
            x2: cx + w / 2.0,
            y2: cy + h / 2.0,
        }
    }

    /// Clamp to image bounds `[0, w) × [0, h)`.
    #[inline]
    pub fn clamp_to(&self, w: f32, h: f32) -> Self {
        Self {
            x1: self.x1.clamp(0.0, w),
            y1: self.y1.clamp(0.0, h),
            x2: self.x2.clamp(0.0, w),
            y2: self.y2.clamp(0.0, h),
        }
    }

    /// Integer pixel rect `(x1, y1, x2, y2)` for raster drawing (half-open).
    #[inline]
    pub fn to_int_rect(&self) -> (i32, i32, i32, i32) {
        (self.x1 as i32, self.y1 as i32, self.x2.ceil() as i32, self.y2.ceil() as i32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElementType {
    Icon,
    Text,
}

/// One parsed screen element. `content` is the caption (Florence-2) or OCR text —
/// `None` in Phase 1 (icons detected but uncaptioned; the brain VLM fills it).
#[derive(Clone, Debug)]
pub struct ParsedElement {
    /// SoM mark number — the red number drawn over the element — 0-based.
    pub mark: usize,
    pub r#type: ElementType,
    pub bbox: BBox,
    pub interactivity: bool,
    pub content: Option<String>,
}

/// Result of [`crate::OmniParser::parse`]: the annotated SoM PNG + the element table.
#[derive(Clone, Debug)]
pub struct SomResult {
    /// Set-of-Mark image, PNG-encoded.
    pub annotated_png: Vec<u8>,
    pub elements: Vec<ParsedElement>,
}
