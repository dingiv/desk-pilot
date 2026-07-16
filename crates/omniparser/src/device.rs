//! Inference device — the seam for *where* YOLO runs.
//!
//! - [`Device::Cpu`] — always works (ort's default execution provider).
//! - [`Device::Cuda`] — the CUDA execution provider. Needs the `cuda` cargo
//!   feature + an onnxruntime-gpu build whose compiled kernels cover your GPU's
//!   compute capability. **Note:** the onnxruntime-gpu binary ort fetches lacks
//!   Blackwell (sm_120) kernels, so on RTX 50-series it errors with
//!   `cudaErrorNoKernelImageForDevice` — use [`Device::TensorRtx`] there.
//! - [`Device::TensorRtx`] — NVIDIA's **TensorRT-RTX** EP (`NvTensorRTRTX`), the
//!   one intended for RTX 50-series / Blackwell via TensorRT. Needs the `nvrtx`
//!   cargo feature + CUDA runtime/cuDNN on the dynamic-linker path (e.g. point
//!   `LD_LIBRARY_PATH` at a torch `nvidia/*/lib` tree). **Caveat (verified):** ort
//!   currently declares this EP Windows-only, so on Linux it silently falls back
//!   to CPU — see `examples/bench.rs` (verified zero GPU usage via `nvidia-smi`).
//!
//! ort silently falls back to CPU if a provider can't *initialize*; if it
//! initializes but a kernel can't run (arch mismatch) it errors per-inference.
//!
//! # Targeting another GPU (the extension point)
//!
//! ort exposes many more providers — DirectML (Windows), CoreML (macOS), ROCm,
//! OpenVINO, …. To target one, add a variant here and return its `ort::ep::*`
//! provider from [`Device::providers`], gated by the matching ort cargo feature
//! (`directml = ["ort/directml"]`, …). Nothing else in the crate changes —
//! [`crate::YoloDetector`] just hands [`Device::providers`] to
//! `SessionBuilder::with_execution_providers`.

use ort::ep::ExecutionProviderDispatch;

#[cfg(feature = "cuda")]
use ort::ep::CUDA;
#[cfg(feature = "nvrtx")]
use ort::ep::NVRTX;

/// Where inference runs. `Default` is [`Device::Cpu`].
#[derive(Clone, Debug, Default)]
pub enum Device {
    /// CPU inference (ort default execution provider).
    #[default]
    Cpu,
    /// NVIDIA GPU via the CUDA execution provider (pre-Blackwell).
    Cuda(CudaOptions),
    /// RTX 50-series / Blackwell via the TensorRT-RTX execution provider.
    TensorRtx(TensorRtxOptions),
}

/// CUDA provider knobs.
#[derive(Clone, Debug, Default)]
pub struct CudaOptions {
    pub device_id: i32,
}

/// TensorRT-RTX provider knobs.
#[derive(Clone, Debug, Default)]
pub struct TensorRtxOptions {
    pub device_id: u32,
}

impl Device {
    /// The ort execution providers to register, in priority order. Empty ⇒ CPU.
    pub fn providers(&self) -> Vec<ExecutionProviderDispatch> {
        match self {
            Device::Cpu => Vec::new(),
            Device::Cuda(opts) => {
                #[cfg(feature = "cuda")]
                {
                    vec![CUDA::default().with_device_id(opts.device_id).build()]
                }
                #[cfg(not(feature = "cuda"))]
                {
                    eprintln!(
                        "[omniparser] CUDA requested (device {}) but the `cuda` feature is off — using CPU",
                        opts.device_id
                    );
                    Vec::new()
                }
            }
            Device::TensorRtx(opts) => {
                #[cfg(feature = "nvrtx")]
                {
                    vec![NVRTX::default().with_device_id(opts.device_id).build()]
                }
                #[cfg(not(feature = "nvrtx"))]
                {
                    eprintln!(
                        "[omniparser] TensorRT-RTX requested (device {}) but the `nvrtx` feature is off — using CPU",
                        opts.device_id
                    );
                    Vec::new()
                }
            }
        }
    }

    /// One-word label for logs.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Device::Cpu => "cpu",
            Device::Cuda(_) => "cuda",
            Device::TensorRtx(_) => "tensorrt-rtx",
        }
    }
}
