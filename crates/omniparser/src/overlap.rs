//! Port of OmniParser's `util/utils.py::remove_overlap_new` — dedup overlapping
//! detection boxes.
//!
//! Phase 1 uses the **icon-only** branch (no OCR boxes yet). The IoU and
//! `is_inside` definitions match the Python exactly: IoU is
//! `max(intersection/union, inter/area_a, inter/area_b)`, and `is_inside(a, b)`
//! holds when `intersection/area(a) > 0.80`.

use crate::types::BBox;

/// Intersection area of two boxes (0 if disjoint).
#[inline]
pub fn intersection(a: &BBox, b: &BBox) -> f32 {
    let x1 = a.x1.max(b.x1);
    let y1 = a.y1.max(b.y1);
    let x2 = a.x2.min(b.x2);
    let y2 = a.y2.min(b.y2);
    (x2 - x1).max(0.0) * (y2 - y1).max(0.0)
}

/// IoU as OmniParser computes it: the max of the standard IoU and each box's
/// containment ratio. This is what makes a tiny box fully inside a big one score ~1.
#[inline]
pub fn iou(a: &BBox, b: &BBox) -> f32 {
    let inter = intersection(a, b);
    let union = a.area() + b.area() - inter + 1e-6;
    let (aa, ab) = (a.area(), b.area());
    let (r1, r2) = if aa > 0.0 && ab > 0.0 {
        (inter / aa, inter / ab)
    } else {
        (0.0, 0.0)
    };
    let base = if union > 0.0 { inter / union } else { 0.0 };
    base.max(r1).max(r2)
}

/// `a` is "mostly inside" `b` (intersection / area(a) > 0.80).
pub fn is_inside(a: &BBox, b: &BBox) -> bool {
    let aa = a.area();
    aa > 0.0 && intersection(a, b) / aa > 0.80
}

/// Icon-only dedup: drop a box when another box overlaps it above `iou_threshold`
/// **and** is smaller (keep the smaller, tighter box). Mirrors `remove_overlap_new`
/// with no `ocr_bbox`.
pub fn remove_overlap(boxes: &[BBox], iou_threshold: f32) -> Vec<BBox> {
    let mut kept = Vec::with_capacity(boxes.len());
    for (i, b1) in boxes.iter().enumerate() {
        let mut valid = true;
        for (j, b2) in boxes.iter().enumerate() {
            if i != j && iou(b1, b2) > iou_threshold && b1.area() > b2.area() {
                valid = false;
                break;
            }
        }
        if valid {
            kept.push(*b1);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(x1: f32, y1: f32, x2: f32, y2: f32) -> BBox {
        BBox { x1, y1, x2, y2 }
    }

    #[test]
    fn disjoint_boxes_have_zero_intersection() {
        let a = b(0.0, 0.0, 10.0, 10.0);
        let z = b(20.0, 20.0, 30.0, 30.0);
        assert_eq!(intersection(&a, &z), 0.0);
        assert_eq!(iou(&a, &z), 0.0);
    }

    #[test]
    fn identical_boxes_have_iou_one() {
        let a = b(0.0, 0.0, 10.0, 10.0);
        assert!((iou(&a, &a) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn tiny_box_inside_big_one_scores_near_one() {
        // OmniParser's IoU takes the max ratio, so containment ~1 even though
        // standard IoU would be small.
        let big = b(0.0, 0.0, 100.0, 100.0);
        let small = b(40.0, 40.0, 60.0, 60.0); // 20x20 inside 100x100
        assert!(iou(&small, &big) > 0.95);
        assert!(is_inside(&small, &big));
        assert!(!is_inside(&big, &small));
    }

    #[test]
    fn remove_overlap_keeps_smaller_of_overlapping_pair() {
        // iou_threshold 0.9 (the OmniParser default for this pass); two boxes that
        // overlap heavily (small inside big): the big one is dropped.
        let big = b(0.0, 0.0, 100.0, 100.0);
        let small = b(40.0, 40.0, 60.0, 60.0);
        let kept = remove_overlap(&[big, small], 0.9);
        assert!(kept.contains(&small));
        assert!(!kept.contains(&big));
    }

    #[test]
    fn remove_overlap_keeps_non_overlapping_boxes() {
        let a = b(0.0, 0.0, 10.0, 10.0);
        let c = b(50.0, 50.0, 60.0, 60.0);
        let kept = remove_overlap(&[a, c], 0.9);
        assert_eq!(kept.len(), 2);
    }
}
