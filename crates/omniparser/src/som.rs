//! Set-of-Mark annotation — port of OmniParser's `util/box_annotator.py`
//! (`BoxAnnotator::annotate` + `get_optimal_label_pos`).
//!
//! Draws a numbered red mark over each element: the bbox outline, a filled label
//! background, and the mark number on top. Pure Rust (`image` + `imageproc` +
//! `ab_glyph`) — visually equivalent to the Python cv2 Hershey rendering, not
//! pixel-identical (acceptable for SoM; the brain VLM reads marks, not subpixels).
//!
//! The label-placement overlap-avoidance tries four positions (top-left,
//! outer-left, outer-right, top-right), skipping any whose background overlaps a
//! detection above IoU 0.3 or leaves the frame — matching the Python.

use ab_glyph::FontRef;
use image::{Rgb, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut, text_size};
use imageproc::rect::Rect;

use crate::overlap::iou;
use crate::types::{BBox, ParsedElement};

/// Bundled font — DejaVuSans (freely redistributable).
const FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

/// Mark/label rendering knobs. Defaults track OmniParser's desktop demo.
#[derive(Clone, Debug)]
pub struct SomConfig {
    /// Bbox outline thickness, in px (Python: 3).
    pub thickness: u32,
    /// Text em-height, in px (Python `text_scale` is cv2 Hershey units; ~16px reads
    /// similarly on a typical desktop screenshot).
    pub text_scale: f32,
    /// Padding around the mark number inside its background, in px (Python: 5).
    pub text_padding: i32,
    /// Mark color (RGB). OmniParser's icon marks render red.
    pub color: Rgb<u8>,
}

impl Default for SomConfig {
    fn default() -> Self {
        Self {
            thickness: 3,
            text_scale: 16.0,
            text_padding: 5,
            color: Rgb([220, 40, 40]),
        }
    }
}

/// Annotate a copy of `img` with a numbered mark per element. Returns the new image.
pub fn annotate(img: &RgbImage, elements: &[ParsedElement], cfg: &SomConfig) -> RgbImage {
    let font = FontRef::try_from_slice(FONT_BYTES).expect("bundled DejaVuSans.ttf must parse");
    let (iw, ih) = (img.width() as i32, img.height() as i32);

    // Detection boxes, used by the label-overlap check.
    let dets: Vec<BBox> = elements.iter().map(|e| e.bbox).collect();

    let mut out = img.clone();
    for e in elements {
        let (x1, y1, x2, y2) = e.bbox.clamp_to(img.width() as f32, img.height() as f32).to_int_rect();
        draw_thick_rect(&mut out, x1, y1, x2, y2, cfg.thickness, cfg.color);

        let label = e.mark.to_string();
        let (tw, th) = text_size(cfg.text_scale, &font, &label);
        let (tw, th) = (tw as i32, th as i32);
        let p = cfg.text_padding;

        let (bx1, by1, bx2, by2) = optimal_label_pos(p, tw, th, x1, y1, x2, &dets, iw, ih);

        // Filled background.
        filled_rect(
            &mut out,
            bx1,
            by1,
            bx2 - bx1,
            by2 - by1,
            cfg.color,
        );

        // Number, inset by padding, contrasted against the background.
        let txt = text_color(&cfg.color);
        let tx = bx1 + p;
        let ty = by1 + p;
        draw_text_mut(&mut out, txt, tx, ty, cfg.text_scale, &font, &label);
    }
    out
}

/// Draw a filled rectangle of arbitrary (possibly negative/large) coords, clamped
/// to the frame. `imageproc`'s `Rect::of_size` panics on non-positive sizes, so we
/// guard + clamp here.
fn filled_rect(img: &mut RgbImage, x: i32, y: i32, w: i32, h: i32, color: Rgb<u8>) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    let x0 = x.clamp(0, iw - 1);
    let y0 = y.clamp(0, ih - 1);
    let x1 = (x + w).clamp(1, iw);
    let y1 = (y + h).clamp(1, ih);
    let rw = (x1 - x0) as u32;
    let rh = (y1 - y0) as u32;
    if rw == 0 || rh == 0 {
        return;
    }
    draw_filled_rect_mut(img, Rect::at(x0, y0).of_size(rw, rh), color);
}

/// Draw a `thickness`-px outline rectangle as four filled bands.
fn draw_thick_rect(img: &mut RgbImage, x1: i32, y1: i32, x2: i32, y2: i32, t: u32, color: Rgb<u8>) {
    let t = t as i32;
    let w = (x2 - x1).max(1);
    let h = (y2 - y1).max(1);
    filled_rect(img, x1, y1, w, t, color); // top
    filled_rect(img, x1, y2 - t, w, t, color); // bottom
    filled_rect(img, x1, y1, t, h, color); // left
    filled_rect(img, x2 - t, y1, t, h, color); // right
}

/// Black or white, whichever contrasts with `color` (luminance > 160 → black).
fn text_color(color: &Rgb<u8>) -> Rgb<u8> {
    let [r, g, b] = color.0;
    let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if lum > 160.0 {
        Rgb([0, 0, 0])
    } else {
        Rgb([255, 255, 255])
    }
}

/// Pick a background rect for the label that avoids overlapping detections and
/// stays in-frame. Returns `(x1, y1, x2, y2)` for the filled background. Port of
/// `box_annotator.get_optimal_label_pos` (its four candidate placements).
fn optimal_label_pos(
    pad: i32,
    tw: i32,
    th: i32,
    x1: i32,
    y1: i32,
    x2: i32,
    dets: &[BBox],
    iw: i32,
    ih: i32,
) -> (i32, i32, i32, i32) {
    // Each candidate: (bg_x1, bg_y1, bg_x2, bg_y2).
    let cands: [(i32, i32, i32, i32); 4] = [
        // top-left
        (x1, y1 - 2 * pad - th, x1 + 2 * pad + tw, y1),
        // outer-left
        (x1 - 2 * pad - tw, y1, x1, y1 + 2 * pad + th),
        // outer-right
        (x2, y1, x2 + 2 * pad + tw, y1 + 2 * pad + th),
        // top-right
        (x2 - 2 * pad - tw, y1 - 2 * pad - th, x2, y1),
    ];

    for c in cands {
        if !overlaps_det_or_frame(c, dets, iw, ih) {
            return c;
        }
    }
    // Fallback: last candidate (top-right), as the Python does.
    cands[3]
}

/// True if the candidate bg rect overlaps any detection > IoU 0.3 or leaves frame.
fn overlaps_det_or_frame(c: (i32, i32, i32, i32), dets: &[BBox], iw: i32, ih: i32) -> bool {
    let (cx1, cy1, cx2, cy2) = c;
    if cx1 < 0 || cy1 < 0 || cx2 > iw || cy2 > ih {
        return true;
    }
    let bg = BBox {
        x1: cx1 as f32,
        y1: cy1 as f32,
        x2: cx2 as f32,
        y2: cy2 as f32,
    };
    dets.iter().any(|d| iou(&bg, d) > 0.3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ElementType;

    fn el(mark: usize, bbox: BBox) -> ParsedElement {
        ParsedElement {
            mark,
            r#type: ElementType::Icon,
            bbox,
            interactivity: true,
            content: None,
        }
    }

    #[test]
    fn annotate_preserves_dimensions_and_paints_mark() {
        let img = RgbImage::from_pixel(200, 150, Rgb([30, 30, 30]));
        let els = vec![el(0, BBox { x1: 10.0, y1: 10.0, x2: 60.0, y2: 60.0 })];
        let out = annotate(&img, &els, &SomConfig::default());
        assert_eq!(out.dimensions(), (200, 150));
        // The red mark color must appear somewhere in the annotated image.
        assert!(out.pixels().any(|p| p.0 == [220, 40, 40]));
    }

    #[test]
    fn text_color_contrasts_with_background() {
        assert_eq!(text_color(&Rgb([220, 40, 40])), Rgb([255, 255, 255])); // red bg → white
        assert_eq!(text_color(&Rgb([240, 240, 240])), Rgb([0, 0, 0])); // light bg → black
    }

    #[test]
    fn label_placed_in_frame_when_room_above() {
        // Box with room above: top-left candidate is in-frame and non-overlapping,
        // so the bg sits above the box.
        let dets = vec![BBox { x1: 100.0, y1: 100.0, x2: 150.0, y2: 150.0 }];
        let (bx1, by1, _bx2, by2) = optimal_label_pos(5, 10, 14, 100, 100, 150, &dets, 400, 400);
        assert!(by1 < 100 && by2 <= 100, "expected bg above the box, got y {by1}..{by2}");
        assert!(bx1 >= 0 && by1 >= 0);
    }
}
