#!/usr/bin/env python3
"""detect_env.py — 探测 GPU / CUDA 工具链 / Python 引擎环境.

跨平台: subprocess + shutil.which + import 探测, 不 shell. 给 remote provider 部署前
看清楚机器能不能跑 vLLM/SGLang, 以及该装什么。

用法:  python detect_env.py
"""
import shutil
import subprocess
import sys


def section(title: str) -> None:
    print(f"=== {title} ===")


def probe_gpu() -> None:
    section("GPU")
    try:
        import pynvml  # type: ignore

        pynvml.nvmlInit()
        for i in range(pynvml.nvmlDeviceGetCount()):
            h = pynvml.nvmlDeviceGetHandleByIndex(i)
            name = pynvml.nvmlDeviceGetName(h)
            if isinstance(name, bytes):
                name = name.decode()
            mem = pynvml.nvmlDeviceGetMemoryInfo(h).total / 1e9
            print(f"  {name} | {mem:.1f} GB")
        pynvml.nvmlShutdown()
        return
    except ImportError:
        pass  # fall through to nvidia-smi
    except Exception as e:  # noqa: BLE001
        print(f"  (pynvml error: {e})")
        return

    if shutil.which("nvidia-smi"):
        out = subprocess.check_output(
            ["nvidia-smi", "--query-gpu=name,memory.total,driver_version", "--format=csv,noheader"],
            text=True,
        )
        for line in out.strip().splitlines():
            print(f"  {line}")
    else:
        print("  (no NVIDIA GPU / nvidia-smi not on PATH)")


def probe_cuda() -> None:
    section("CUDA toolkit")
    if shutil.which("nvcc"):
        out = subprocess.check_output(["nvcc", "--version"], text=True)
        for line in out.strip().splitlines():
            print(f"  {line}")
    else:
        print("  (nvcc not on PATH; CUDA toolkit not installed)")


def probe_pythons() -> None:
    section("Python engines")
    for mod in ("vllm", "sglang", "fastapi", "uvicorn", "torch", "huggingface_hub"):
        try:
            m = __import__(mod)
            ver = getattr(m, "__version__", "?")
            print(f"  {mod}: {ver}")
        except ImportError:
            print(f"  {mod}: (not installed)")


def main() -> int:
    probe_gpu()
    probe_cuda()
    probe_pythons()
    print("=== python ===")
    print(f"  {sys.version.splitlines()[0]}  ({sys.executable})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
