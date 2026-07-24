#!/usr/bin/env python3
"""serve.py — 起 OpenAI 兼容的本地模型服务, 供 dp-models 的 remote provider 调用。

4 个子命令:
  asr    — Qwen3-ASR (qwen-asr PyTorch 后端 + FastAPI, /v1/audio/transcriptions)
  vllm   — vLLM (api_server.main, /v1/chat/completions)
  sglang — SGLang (launch_server, /v1/chat/completions)
  mock   — 固定响应 (FastAPI, 测试 remote 链路)

用法:
  python serve.py asr    --model assets/models/qwen3-asr-0.6b-hf --port 8080
  python serve.py vllm   --model Qwen/Qwen3-1.7B --port 8000
  python serve.py sglang --model .../Qwen3-1.7B-Q8_0.gguf --port 8700
  python serve.py mock   --port 8000
"""
import argparse
import sys
from typing import List


def serve_asr(model: str, port: int) -> int:
    """Qwen3-ASR via qwen-asr PyTorch 后端 + FastAPI (/v1/audio/transcriptions)。
    不走 vllm (避免 transformers 5.x 版本冲突)。需: qwen-asr + fastapi + uvicorn。"""
    import os
    import tempfile
    import torch
    from qwen_asr import Qwen3ASRModel
    from fastapi import FastAPI, UploadFile, File
    from fastapi.responses import JSONResponse
    import uvicorn

    m = Qwen3ASRModel.from_pretrained(
        model, dtype=torch.float16, device_map="cuda",
        max_inference_batch_size=1, max_new_tokens=256)

    app = FastAPI(title="dp-models ASR (qwen-asr PyTorch)")

    @app.post("/v1/audio/transcriptions")
    async def transcribe(file: UploadFile = File(...)):
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as f:
            f.write(await file.read())
            path = f.name
        try:
            r = m.transcribe(path)
            return JSONResponse({"text": r[0].text})
        finally:
            os.unlink(path)

    @app.get("/health")
    def health():
        return JSONResponse({"status": "ok"})

    print(f"[serve] ASR (qwen-asr PyTorch) on http://0.0.0.0:{port} model={model}", flush=True)
    uvicorn.run(app, host="0.0.0.0", port=port)
    return 0


def serve_vllm(model: str, port: int, extra: List[str]) -> int:
    from vllm.entrypoints.openai import api_server
    api_server.main(["--model", model, "--port", str(port), *extra])
    return 0


def serve_sglang(model: str, port: int, extra: List[str]) -> int:
    from sglang.srt.server_args import ServerArgs
    from sglang.srt.entrypoints.http_server import launch_server
    args = ServerArgs(model_path=model, port=port, host="0.0.0.0")
    if extra:
        import runpy
        sys.argv = ["sglang.launch_server", "--model-path", model,
                    "--port", str(port), "--host", "0.0.0.0", *extra]
        runpy.run_module("sglang.launch_server", run_name="__main__", alter_sys=True)
        return 0
    launch_server(args)
    return 0


def serve_mock(port: int) -> int:
    from fastapi import FastAPI
    from fastapi.responses import JSONResponse
    import uvicorn

    app = FastAPI(title="dp-models mock")

    @app.post("/v1/audio/transcriptions")
    def transcribe():
        return JSONResponse({"text": "[mock transcript]"})

    @app.post("/v1/chat/completions")
    def chat():
        return JSONResponse({"choices": [{"message": {"content": "[mock llm response]"}, "index": 0}]})

    @app.get("/health")
    def health():
        return JSONResponse({"status": "ok"})

    print(f"[serve] mock on http://0.0.0.0:{port}", flush=True)
    uvicorn.run(app, host="0.0.0.0", port=port)
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description="起 OpenAI 兼容本地模型服务 (asr / vllm / sglang / mock).")
    sub = p.add_subparsers(dest="engine", required=True)

    for eng in ("asr", "vllm", "sglang"):
        sp = sub.add_parser(eng, help=f"起 {eng} 服务")
        sp.add_argument("--model", required=True, help="模型目录 / id / 路径")
        sp.add_argument("--port", type=int, default=8000)
        if eng != "asr":
            sp.add_argument("extra", nargs=argparse.REMAINDER, help="透传给引擎的额外参数")

    mp = sub.add_parser("mock", help="起固定响应 mock")
    mp.add_argument("--port", type=int, default=8000)

    args = p.parse_args()
    if args.engine == "asr":
        return serve_asr(args.model, args.port)
    if args.engine == "vllm":
        return serve_vllm(args.model, args.port, args.extra)
    if args.engine == "sglang":
        return serve_sglang(args.model, args.port, args.extra)
    if args.engine == "mock":
        return serve_mock(args.port)
    return 1


if __name__ == "__main__":
    sys.exit(main())
