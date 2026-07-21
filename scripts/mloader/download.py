#!/usr/bin/env python3
"""download.py — 下载 HuggingFace 模型 (huggingface_hub, 断点续传 + sha 校验).

比裸 curl 可靠得多 (curl 对 HF 大文件在本容器里会 ~3GB 处截断却报成功). 支持 repo 快照
或单文件.

用法:
  python download.py --repo mistralai/Voxtral-Mini-4B-Realtime-2602 --local-dir models/voxtral
  python download.py --repo mistralai/Voxtral-Mini-4B-Realtime-2602 --file consolidated.safetensors --local-dir models/voxtral
"""
import argparse
import sys


def main() -> int:
    p = argparse.ArgumentParser(description="下载 HuggingFace 模型 (断点续传 + 校验).")
    p.add_argument("--repo", required=True, help="HuggingFace repo id, e.g. Qwen/Qwen3-ASR-0.6B")
    p.add_argument("--local-dir", required=True, help="本地目标目录")
    p.add_argument("--file", help="只下单个文件 (不填 = 整个 repo snapshot)")
    args = p.parse_args()

    try:
        from huggingface_hub import hf_hub_download, snapshot_download
    except ImportError:
        print("ERROR: pip install huggingface_hub", file=sys.stderr)
        return 1

    if args.file:
        path = hf_hub_download(args.repo, args.file, local_dir=args.local_dir)
    else:
        path = snapshot_download(args.repo, local_dir=args.local_dir)
    print(f"downloaded {args.repo} -> {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
