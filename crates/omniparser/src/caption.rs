//! **Reserved seam (Phase 3).** Icon captioning — describe each icon's function.
//!
//! The Python pipeline crops each uncaptioned icon, resizes to 64×64, and runs
//! Florence-2 with the `<CAPTION>` prompt (`util/utils.py::get_parsed_content_icon`).
//! The Rust port will run Florence-2's ONNX export (5 subgraphs: vision encoder,
//! embed tokens, encoder, decoder, decoder-merged) over `ort` + `tokenizers`,
//! driving the autoregressive generate loop host-side. Heavy and unproven in Rust
//! here, so deferred; the brain VLM captions icons from the SoM image meanwhile.
//!
//! Not wired this round. The trait mirrors how it plugs into [`crate::OmniParser`].

use crate::types::BBox;

/// Caption a set of icon crops. Implementations run a Florence-2 ONNX loop (Phase 3).
pub trait Captioner {
    /// Caption each box (order preserved). `None` ⇒ no caption produced for that box.
    fn caption(&self, image: &image::RgbImage, boxes: &[BBox]) -> Vec<Option<String>>;
}
