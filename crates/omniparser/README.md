# `vrover-omniparser` — OmniParser, in Rust

A Rust port of [Microsoft OmniParser](../../OmniParser)'s screen-parsing pipeline.
A screenshot goes in; an annotated **Set-of-Mark** image (each UI element numbered)
and an element table come out — the grounding half of a visual GUI agent.

**Phase 1 (this crate, now):** YOLO icon detection (ONNX Runtime) + SoM
annotation. Icons are detected and numbered but **uncaptioned** (`content = None`);
the brain VLM (GLM-4.6V / Claude) reads the marks and captions them itself.

| stage | status | backend |
|---|---|---|
| Icon detection (YOLO) | ✅ Phase 1 | `onnx-community/OmniParser-v2.0_icon_detect` via `ort` (CPU) |
| Box dedup (`remove_overlap`) | ✅ Phase 1 | port of `util/utils.py` |
| SoM annotation (`BoxAnnotator`) | ✅ Phase 1 | port of `util/box_annotator.py` (`image` + `imageproc`) |
| OCR (text boxes) | 🔜 Phase 2 | `ocr::Ocr` trait reserved → RapidOCR ONNX |
| Icon captioning (Florence-2) | 🔜 Phase 3 | `caption::Captioner` trait reserved → Florence-2 ONNX |

## Layout

```
src/
  lib.rs      OmniParser + OmniParserConfig; parse() = detect → dedup → annotate
  types.rs    BBox, ElementType, ParsedElement, SomResult
  yolo.rs     YoloDetector: letterbox → ort run → decode+NMS → un-letterbox
  overlap.rs  remove_overlap (IoU/inside dedup, port of remove_overlap_new)
  som.rs      annotate (BoxAnnotator port: numbered marks + placement avoidance)
  ocr.rs      reserved seam (Phase 2)
  caption.rs  reserved seam (Phase 3)
  error.rs    OmniError (ort errors flattened to Display strings)
examples/parse_one.rs   one-shot: PNG → SoM PNG + element table
assets/DejaVuSans.ttf    bundled font for mark numbers
fetch_weights.sh         download the icon-detect ONNX (~80MB) into weights/
```

## Run

```bash
./fetch_weights.sh                                                          # one-time, ~80MB
cargo run -p vrover-omniparser --example parse_one -- <image.png> out.png   # → out.png + element table
```

`OMNIPARSER_WEIGHTS=<path>` overrides the weights location (defaults to this
crate's `weights/icon_detect.onnx`).

## Use as a library

```rust
use vrover_omniparser::{OmniParser, OmniParserConfig};

let mut parser = OmniParser::new(OmniParserConfig::new("weights/icon_detect.onnx"))?;
let som = parser.parse(&rgb_image)?;     // RgbImage → SomResult
// som.annotated_png : Vec<u8>   (PNG bytes)
// som.elements[i].mark / .bbox  (xyxy, pixel coords) / .content (None in Phase 1)
```

## Porting notes (faithfulness)

- **YOLO I/O** (verified by loading the model): input `images [1,3,640,640]` f32
  RGB /255; output `output0 [1,5,8400]` = `[cx,cy,w,h,conf]` (single class). The
  export may or may not pre-sigmoid the score — detected at runtime (max raw > 1.5
  ⇒ sigmoid). Thresholds default to `omniparserserver`'s `box=0.05`, and the IoU
  0.1 `get_som_labeled_img` passes to `predict_yolo`.
- **`remove_overlap`** matches `remove_overlap_new` (IoU = max of standard IoU and
  the two containment ratios; `is_inside` threshold 0.80; keep-smaller-box).
- **SoM** matches `BoxAnnotator` + `get_optimal_label_pos` geometry (4 candidate
  label placements, skip on >0.3 IoU overlap or out-of-frame). Rendering uses
  `imageproc`+`ab_glyph` instead of cv2 Hershey, so it is visually equivalent, not
  pixel-identical.

## GPU (not yet)

CPU-first by design: `ort`'s CUDA execution provider has no verified prebuilt for
Blackwell (sm_120 / RTX 5070 Ti), so correctness was established on CPU. Enabling
GPU is a follow-up: ort `cuda` feature + a Blackwell-compatible `onnxruntime-gpu`.
The decode/NMS/annotation code is device-agnostic — only the session build changes.
