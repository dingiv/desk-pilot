#!/usr/bin/env bash
# Link sherpa CUDA providers + CUDA/cuDNN runtime libs into cargo's target dirs.
#
# Why: sherpa-onnx-sys's build.rs copies libonnxruntime.so into target/{debug,release}/, so at
# runtime ONNX Runtime's $ORIGIN is that target dir — but it then can't find providers_*.so or
# the CUDA/cuDNN libs (which live in the sherpa GPU bundle / omniparser/lib). This script
# symlinks them next to the copied libonnxruntime.so so $ORIGIN resolves everything.
#
# Run once after building with the GPU lib (desk-pilot/lib → sherpa-...-gpu). Re-run after a
# `cargo clean` or when switching between debug/release.
#
# Then set aura.json `"asr_provider": "cuda"` for SenseVoice (works). Qwen3-ASR stays "cpu"
# (cuDNN 9.1 / sm_120 mis-decodes Chinese — see crates/aura-asr warning).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHERPA="$ROOT/assets/sherpa/sherpa-onnx-v1.13.4-cuda-12.x-cudnn-9.x-linux-x64-gpu/lib"
OMP="$ROOT/crates/omniparser/lib"
PROVS="libonnxruntime_providers_cuda.so libonnxruntime_providers_shared.so libonnxruntime_providers_tensorrt.so"
CUDA_LIBS="libcudart.so.12 libcublas.so.12 libcublasLt.so.12 libcufft.so.11 libcurand.so.10 \
libcusolver.so.11 libcusparse.so.12 libnvrtc.so.12 libnvrtc-builtins.so.12.8 libnvJitLink.so.12 \
libnvToolsExt.so.1 libcudnn.so.9 libcudnn_graph.so.9 libcudnn_cnn.so.9 libcudnn_ops.so.9 \
libcudnn_engines_precompiled.so.9 libcudnn_engines_runtime_compiled.so.9 libcudnn_heuristic.so.9 \
libcudnn_adv.so.9"
for dir in "$ROOT/target/debug" "$ROOT/target/debug/examples" \
           "$ROOT/target/release" "$ROOT/target/release/examples"; do
    [ -d "$dir" ] || continue
    for f in $PROVS; do ln -sf "$SHERPA/$f" "$dir/$f"; done
    for f in $CUDA_LIBS; do [ -e "$OMP/$f" ] && ln -sf "$OMP/$f" "$dir/$f"; done
    echo "linked → $dir"
done
echo "done. Set aura.json asr_provider=cuda for SenseVoice; Qwen3-ASR stays cpu."
