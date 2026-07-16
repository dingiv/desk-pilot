# `vrover-omniparser` performance

Per-stage timings (measured 2026-06-26 on `OmniParser/imgs/excel.png`, 1919×1079;
debug build, single worker). Re-measure any time with `OMNIPARSER_TIMING=1`.

## Why CPU-side dominates (the Amdahl story)

Inference is a *small* slice; the screenshot is large (1920×1080) and the
surrounding CPU work — resize + tensorize + PNG — is what eats the budget. GPU-
accelerating inference alone barely dents it.

| stage | CPU before | CPU after Tier 0+1 | CUDA (steady) | what |
|---|---|---|---|---|
| resize (letterbox) | 275 ms | **145 ms** | 145 ms | downscale 1920×1080 → ~640×360 |
| to_nchw + /255 | 151 ms | **4.6 ms** | 4.6 ms | HWC u8 → NCHW f32 padded tensor |
| infer (YOLO fwd) | 77 ms | 77 ms | **6 ms** | ort forward (CPU vs CUDA EP) |
| post (NMS) | 0.6 ms | 0.6 ms | 0.6 ms | box decode + NMS |
| annotate (SoM) | 9 ms | 9 ms | 9 ms | draw boxes + numbers |
| PNG encode | 363 ms | **119 ms** | 122 ms | zlib + filter over 6.2 MP |
| **total** | **875 ms** | **355 ms** | **~288 ms (p50)** | |

**Tier 0+1 net: CPU 0.9 → 2.3 img/s (2.5×); CUDA now clearly beats CPU (~288 vs 355).**

### What Tier 0+1 changed (and the gotchas)
- **to_nchw 151 → 4.6 ms**: stopped using `ndarray`'s `arr[[0,0,y,x]]` indexer
  (per-access bounds/dim checks × ~700 k writes). Build a flat C-contiguous
  `Vec<f32>` with direct indexing, then `Array4::from_shape_vec`.
- **PNG 363 → 119 ms**: `image`'s default `write_to(Png)` uses `Adaptive` filtering
  (tries 5 filters per scanline). Switched to `PngEncoder` with
  `FilterType::NoFilter` + `CompressionType::Fast`.
- **resize 275 → 145 ms**: `image`'s `imageops::resize(Triangle)` is scalar.
  Switched to `fast_image_resize` — **but its default `Resizer` is Lanczos3**
  (slower than scalar Triangle!). Must pass
  `ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))`.

## Remaining bottleneck
`resize` (145 ms, ~50%) + `png` (122 ms, ~40%) are still the bulk on both devices.
They're CPU work on the full-resolution image; neither benefits from the GPU EP.

## TODO(Tier 1.5) — more CPU wins (not done)
- `resize`: `fast_image_resize`'s SIMD is fastest on **U8x4**; we resize **U8x3**
  (RGB). Resizing as RGBA (dummy alpha) likely drops 145 ms toward ~40 ms. Or
  `ResizeAlg::Nearest` (~15 ms, slight detection-quality risk).
- `png`: switch to **JPEG q90** for the SoM image (VLM reads JPEG fine; encode is
  ~40 ms vs PNG's 119 ms and far smaller bytes for base64 transport).

## TODO(Tier 2) — move preproc onto the GPU (not done)
**Goal:** fold resize + pad + transpose + normalize into the ONNX model graph so
ort's CUDA EP runs the whole pipeline on-GPU, fused with inference — ~150 ms of
CPU resize+tensorize → ~0, plus no host↔device ping-pong.

**Plan:** do one-time **model surgery** (Python `onnx` helper, run offline) that
prepends a preproc subgraph to `icon_detect.onnx`:

```
input: image  HWC u8  (1, H, W, 3)
  → Resize   (scale longest side → 640, keep aspect)   [ONNX Resize, antialias]
  → Pad      (center to 640×640, value 114)            [ONNX Pad]
  → Transpose(HWC → CHW)                                [ONNX Transpose]
  → Mul      (× 1/255)                                  [ONNX Mul]
  → <existing icon_detect graph>  (expects [1,3,640,640] f32)
```

All five ops are standard ONNX the CUDA EP accelerates with NVIDIA's tuned kernels
— **no hand-written CUDA**, no `nvcc` (absent in this env). The host then uploads
the raw image once (PCIe copy of ~6 MB ≈ 0.3 ms, negligible) and the GPU does the
rest.

**Caveats to solve when implementing:**
- Letterbox scale depends on input aspect ratio → `Resize` with dynamic target
  sizes (pass `scales`/`sizes` as graph inputs), or letterbox to a fixed scheme.
- Verify the CUDA EP covers `Resize`+`Pad` for the chosen opset (opset 18+ is safe).
- Re-benchmark: after this, inference (6 ms GPU) + png (119 ms, or ~40 JPEG) is
  the floor; pair with **batch inference** to amortize launch + first-run cuDNN
  warmup (~430 ms one-shot).

**Why not hand-write a CUDA 算子?** ort's CUDA EP already ships tuned Resize/
Pad/Transpose/Mul kernels; a custom kernel (via `cust`/`cudarc` or an ONNX custom
op) would be more code, worse perf, and needs `nvcc`/`nvrtc` (not installed).
Custom kernels are only worth it for op-coverage gaps — there are none here.
