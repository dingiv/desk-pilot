//! `omniparser` — a Rust port of OmniParser's screen-parsing pipeline.
//!
//! **Phase 1 (this round):** YOLO icon detection (ONNX Runtime) + Set-of-Mark
//! annotation. A screenshot goes in; an annotated SoM image (each icon numbered)
//! + an element table come out. Icons are detected but **uncaptioned**
//! (`content = None`) — the brain VLM fills captions from the SoM image.
//!
//! Reserved seams for later phases: [`ocr::Ocr`] (Phase 2, RapidOCR ONNX) and
//! [`caption::Captioner`] (Phase 3, Florence-2 ONNX generate loop). They drop into
//! [`OmniParser::parse`] without restructuring.
//!
//! Ported from Microsoft's OmniParser (`util/utils.py` + `util/box_annotator.py`).

use std::path::PathBuf;
use std::time::Instant;

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageEncoder, RgbImage};

pub mod caption;
pub mod device;
pub mod error;
pub mod ocr;
pub mod overlap;
pub mod som;
pub mod types;
pub mod yolo;

pub use device::{CudaOptions, Device, TensorRtxOptions};
pub use error::{OmniError, Result};
pub use som::SomConfig;
pub use types::{BBox, ElementType, ParsedElement, SomResult};
pub use yolo::{Detection, YoloDetector};

/// Emit per-stage timing to stderr when `OMNIPARSER_TIMING` is set (read once).
pub(crate) fn timing() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNIPARSER_TIMING").is_ok())
}

/// Knobs for [`OmniParser`]. `yolo_path` points at the icon-detect ONNX (fetch
/// via `fetch_weights.sh`).
#[derive(Clone, Debug)]
pub struct OmniParserConfig {
    pub yolo_path: PathBuf,
    pub box_threshold: f32,
    pub iou_threshold: f32,
    pub som: SomConfig,
    /// Where YOLO runs. Default [`Device::Cpu`].
    pub device: Device,
    /// ort intra-op threads per session. `None` = ort default; pass `Some(1)` per
    /// worker in a parallel pool to avoid thread oversubscription.
    pub intra_threads: Option<usize>,
}

impl OmniParserConfig {
    /// Defaults: `box_threshold=0.05`, `iou_threshold=0.1` (matching
    /// `omniparserserver`), the standard SoM look, CPU, ort-default threads.
    pub fn new(yolo_path: impl Into<PathBuf>) -> Self {
        Self {
            yolo_path: yolo_path.into(),
            box_threshold: 0.05,
            iou_threshold: 0.1,
            som: SomConfig::default(),
            device: Device::default(),
            intra_threads: None,
        }
    }
}

/// The OmniParser: holds the loaded YOLO session + render config.
#[derive(Debug)]
pub struct OmniParser {
    yolo: YoloDetector,
    som: SomConfig,
    /// IoU threshold for the post-detection `remove_overlap` dedup (Python default 0.9).
    overlap_iou: f32,
}

impl OmniParser {
    pub fn new(cfg: OmniParserConfig) -> Result<Self> {
        let yolo = YoloDetector::new(&cfg.yolo_path, cfg.device.clone(), cfg.intra_threads)?
            .with_thresholds(cfg.box_threshold, cfg.iou_threshold);
        Ok(Self {
            yolo,
            som: cfg.som,
            overlap_iou: 0.9,
        })
    }

    /// Parse one screenshot: detect icons → dedup overlapping boxes → annotate.
    pub fn parse(&mut self, rgb: &RgbImage) -> Result<SomResult> {
        let on = timing();
        let t0 = Instant::now();
        let dets = self.yolo.detect(rgb)?;
        let t1 = Instant::now();
        let boxes: Vec<BBox> = dets.iter().map(|d| d.bbox).collect();
        let kept = overlap::remove_overlap(&boxes, self.overlap_iou);

        let elements: Vec<ParsedElement> = kept
            .into_iter()
            .enumerate()
            .map(|(mark, bbox)| ParsedElement {
                mark,
                r#type: ElementType::Icon,
                bbox,
                interactivity: true,
                content: None,
            })
            .collect();

        let annotated = som::annotate(rgb, &elements, &self.som);
        let t2 = Instant::now();
        let mut buf = std::io::Cursor::new(Vec::new());
        // Tier 0: no PNG row-filtering + Fast zlib. `Adaptive` tries 5 filters per
        // row (slow); NoOp skips filtering entirely. Lossless, cheap, bigger file —
        // fine, the SoM image is read by a VLM, not archived.
        PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::NoFilter)
            .write_image(
                annotated.as_raw(),
                annotated.width(),
                annotated.height(),
                image::ExtendedColorType::Rgb8,
            )?;
        let t3 = Instant::now();

        if on {
            eprintln!(
                "    [timing] (detect {:7.2}ms | annotate {:6.2}ms | png {:6.2}ms)",
                t1.duration_since(t0).as_secs_f64() * 1000.0,
                t2.duration_since(t1).as_secs_f64() * 1000.0,
                t3.duration_since(t2).as_secs_f64() * 1000.0,
            );
        }
        Ok(SomResult {
            annotated_png: buf.into_inner(),
            elements,
        })
    }
}
