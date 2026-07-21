#!/usr/bin/env python3
"""serve.py — 起一个 OpenAI 兼容的本地模型服务 (vLLM / SGLang), 供 dp-models 的 remote
provider (HttpLlm / HttpAsr) 连接。也提供一个内嵌 mock 用于离线测试 remote 链路。

跨平台: 纯 subprocess + argparse, 不 shell (Windows 友好)。

用法:
  python serve.py vllm   --model Qwen/Qwen3-1.7B --port 8000
  python serve.py sglang --model Qwen/Qwen3-1.7B --port 8000 [--extra --trust-remote-code]
  python serve.py mock   --port 8000

需先装对应引擎:  pip install vllm  |  pip install "sglang[all]"  |  pip install fastapi uvicorn
"""
import argparse
import subprocess
import sys
from typing import List


def _run(cmd: List[str]) -> int:
    print("[serve] exec:", " ".join(cmd), flush=True)
    return subprocess.call(cmd)


def serve_vllm(model: str, port: int, extra: List[str]) -> int:
    # `python -m vllm.entrypoints.openai.api_server` — 不依赖 PATH 上的 `vllm` 命令。
    cmd = [sys.executable, "-m", "vllm.entrypoints.openai.api_server",
           "--model", model, "--port", str(port), *extra]
    return _run(cmd)


def serve_sglang(model: str, port: int, extra: List[str]) -> int:
    cmd = [sys.executable, "-m", "sglang.launch_server",
           "--model-path", model, "--port", str(port), *extra]
    return _run(cmd)


def serve_mock(port: int) -> int:
    """内嵌 FastAPI OpenAI 兼容 mock (固定文本) —— 供 dp-models remote 测试, 无需真模型。

    import 放函数内: 只有走 mock 才需要 fastapi/uvicorn, vllm/sglang 路径不强制装它。
    """
    from fastapi import FastAPI
    from fastapi.responses import JSONResponse
    import uvicorn

    app = FastAPI(title="dp-models mock")

    @app.post("/v1/audio/transcriptions")
    def transcribe():  # type: ignore[no-untyped-def]
        return JSONResponse({"text": "[mock transcript]"})

    @app.post("/v1/chat/completions")
    def chat():  # type: ignore[no-untyped-def]
        return JSONResponse({"choices": [{"message": {"content": "[mock llm response]"}, "index": 0}]})

    @app.get("/health")
    def health():  # type: ignore[no-untyped-def]
        return JSONResponse({"status": "ok"})

    print(f"[serve] mock OpenAI server on http://0.0.0.0:{port}", flush=True)
    uvicorn.run(app, host="0.0.0.0", port=port)
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description="起 OpenAI 兼容本地模型服务 (vllm / sglang / mock).")
    sub = p.add_subparsers(dest="engine", required=True)

    for eng in ("vllm", "sglang"):
        sp = sub.add_parser(eng, help=f"起 {eng} 服务")
        sp.add_argument("--model", required=True, help="模型 id / 路径")
        sp.add_argument("--port", type=int, default=8000)
        sp.add_argument("extra", nargs=argparse.REMAINDER, help="透传给引擎的额外参数 (以 -- 开头)")

    mp = sub.add_parser("mock", help="起固定响应 mock (测试 remote 链路)")
    mp.add_argument("--port", type=int, default=8000)

    args = p.parse_args()
    if args.engine == "vllm":
        return serve_vllm(args.model, args.port, args.extra)
    if args.engine == "sglang":
        return serve_sglang(args.model, args.port, args.extra)
    if args.engine == "mock":
        return serve_mock(args.port)
    return 1


if __name__ == "__main__":
    sys.exit(main())
