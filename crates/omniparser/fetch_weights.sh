#!/usr/bin/env bash
# Download the YOLO icon-detect ONNX model into ./weights.
#   onnx-community/OmniParser-v2.0_icon_detect  (AGPL-3.0, ~80MB)
# HuggingFace is reachable via plain curl here (no `hf` CLI needed).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mkdir -p "$HERE/weights"

URL="https://huggingface.co/onnx-community/OmniParser-v2.0_icon_detect/resolve/main/onnx/model.onnx"
OUT="$HERE/weights/icon_detect.onnx"

if [[ -s "$OUT" ]]; then
  echo "[fetch_weights] already present: $OUT ($(stat -c%s "$OUT") bytes)"
  exit 0
fi

echo "[fetch_weights] downloading $URL ..."
curl -fL "$URL" -o "$OUT"
echo "[fetch_weights] ok: $OUT ($(stat -c%s "$OUT") bytes)"
echo "  then: cargo run -p vrover-omniparser --example parse_one -- <image.png> out_som.png"
