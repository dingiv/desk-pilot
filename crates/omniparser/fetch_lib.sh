#!/usr/bin/env bash
# Populate crates/omniparser/lib with a Blackwell (sm_120)-capable onnxruntime-gpu
# (CUDA-12 build) + its CUDA/cuDNN runtime deps, for ort `load-dynamic`.
#
# Why this exists: ort's *downloaded* onnxruntime-gpu lacks sm_120 kernels, so on
# RTX 50-series the CUDA EP errors `cudaErrorNoKernelImageForDevice`. The
# onnxruntime-gpu **1.25.1** pip wheel is both the CUDA-12 build (matches a torch
# cu128 env: libcudart.so.12 / libcudnn.so.9) AND includes Blackwell — so we borrow
# it. (1.27's pip wheel is CUDA-13 — needs .so.13 we don't have; the CUDA-12 build
# of 1.27 is GitHub-tgz only. 1.25.1 is the last CUDA-12-default pip release.)
#
# CUDA/cuDNN libs come from a torch `nvidia-*` wheel tree (override NV_LIBS).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB="$HERE/lib"
NV="${NV_LIBS:-/workspaces/gui_agent/OmniParser/.venv/lib/python3.13/site-packages/nvidia}"
PIP="${PIP:-python3 -m pip}"

echo "[fetch_lib] onnxruntime-gpu 1.25.1 (CUDA-12, Blackwell) → $LIB"
mkdir -p "$LIB"
rm -f "$LIB"/libonnxruntime*   # fresh ORT libs (keep CUDA deps if re-run)

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
$PIP download onnxruntime-gpu==1.25.1 -d "$TMP" --no-deps --only-binary :all: >/dev/null
W="$(ls "$TMP"/*.whl | head -1)"

# Engine + providers (skip the pybind11 Python binding).
unzip -o -j "$W" \
  'onnxruntime/capi/libonnxruntime.so.1.25.1' \
  'onnxruntime/capi/libonnxruntime_providers_*.so' \
  -d "$LIB" >/dev/null
( cd "$LIB" && ln -sf libonnxruntime.so.1.25.1 libonnxruntime.so )

# CUDA runtime deps from the torch nvidia-* wheels (preserve soname symlinks).
echo "[fetch_lib] CUDA/cuDNN deps from $NV"
cp -a "$NV"/*/lib/*.so* "$LIB"/
# Drop single-GPU-irrelevant libs (saves ~1.2GB; NCCL/NVSHMEM are multi-GPU).
rm -f "$LIB"/libnccl.so* "$LIB"/libnvshmem_host.so* \
      "$LIB"/libcusparseLt.so* "$LIB"/libcusolverMg.so*

echo "[fetch_lib] done: $(du -sh "$LIB" | cut -f1), $(ls "$LIB" | wc -l) files"
echo "  build:  cargo build -p vrover-omniparser --no-default-features --features load-dynamic,cuda --example bench"
echo "  run:    LD_LIBRARY_PATH=$LIB <bench> <imgs> --device cuda"
