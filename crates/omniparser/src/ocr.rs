//! **Reserved seam (Phase 2).** OCR — detect + recognize text boxes.
//!
//! The Python pipeline runs EasyOCR (`util/utils.py::check_ocr_box`); the Rust
//! port will back this with RapidOCR's ONNX models (DB detection + CRNN
//! recognition) over `ort`. Text elements then flow into `remove_overlap` as the
//! `ocr_bbox` argument the Python passes.
//!
//! Not wired this round — `OmniParser::parse` produces icon elements only. The
//! trait is here so a later phase drops in without restructuring [`crate::OmniParser`].

use crate::types::BBox;

/// One OCR hit: the recognized text + its box (pixel xyxy).
#[derive(Clone, Debug)]
pub struct OcrHit {
    pub text: String,
    pub bbox: BBox,
}

/// Detect + recognize text. Implementations run an ONNX OCR model (Phase 2).
pub trait Ocr {
    fn read(&self, image: &image::RgbImage) -> Vec<OcrHit>;
}
