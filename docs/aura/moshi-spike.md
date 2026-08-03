# Moshi 实时语音模型 Spike（2026-07-31）

> 状态：🔴 **搁置**。16GB VRAM 跑不了 Moshi 7B。探索记录在此，等硬件升级或
> Rust candle Q8 路径解锁后再继续。

## 目标

验证 Moshi（Kyutai 全双工实时语音对话模型）在 desk-pilot 环境的可行性：
- 200ms 端到端延迟（vs 当前级联管道 ~2.2s）
- 全双工打断（barge-in，AI 秘书圣杯）
- Rust/candle 原生集成（进 `crates/aura-asr/`）

## 环境

- GPU: RTX 5070 Ti **16GB**（Blackwell sm_120）
- CUDA: 13.2 toolkit
- Rust: candle 0.10+（mistral.rs 用的）/ candle 0.9.1（Moshi Rust 后端用的）
- Python: torch 2.9.1+cu128

## 尝试的路径与结果

### 1. Python BF16 GPU → ❌ OOM

```bash
python -m moshi.server --hf-repo kyutai/moshiko-pytorch-bf16
```

- 模型权重 ~14GB（BF16 7B 参数）
- 加载到 14.27GB 时 OOM（剩 57MB）
- **16GB 显存不够**——权重 + warmup activations 超了

### 2. Python Q8 GPU/CPU → ❌ bitsandbytes dtype bug

```bash
python -m moshi.server --hf-repo kyutai/moshiko-pytorch-q8
```

```
RuntimeError: Expected `weight_scb` to have type float, but got bfloat16.
When using quantized models, care should be taken not to change the dtype
of the model once initialized.
```

- Q8 量化模型（~7GB）在加载后 warmup 时炸
- bitsandbytes 期望 float32 scale tensor，模型存的是 bfloat16
- PyPI moshi 包的 Q8 支持标记为 "experimental"，确认不可用
- CPU 模式同样失败（dtype bug 与设备无关）

### 3. Rust candle GPU → ❌ 编译失败

```bash
cd moshi/rust && cargo build --features cuda --bin moshi-backend -r
```

三个独立编译问题：

| 问题 | 根因 | Patch 尝试 |
|---|---|---|
| **cudarc 0.16.2** 不认 CUDA 13.2 | 版本检查硬编码到 12.8 | ✅ Patched build.rs（13.2→映射 12.8）|
| **sentencepiece-sys** C++ 编译 | GCC 15 严格声明匹配（`no declaration matches`） | ⚠️ CXXFLAGS 无效（build.rs 不传） |
| **candle-kernels** sm_120 | candle 0.9.1 无 Blackwell CUDA kernel | ❌ 无法 patch（需 candle 升级） |

### 4. Rust candle CPU → ❌ sentencepiece-sys

CPU 模式不需要 candle-kernels（CUDA），但 sentencepiece-sys 的 C++ 编译仍失败。
系统安装 `libsentencepiece-dev` 也不行（-sys crate 从源码编译，不用系统库）。

## 根因分析

### 显存瓶颈（Python 路径）

Moshi 7B 参数量 × BF16 = 14GB 权重。RTX 5070 Ti 16GB 扣除系统占用（~0.5GB）
后可用 ~15.5GB。加载 14GB 权重后剩余 ~1.5GB，不够 warmup（transformer 前向
需要 KV cache + activations）。

**结论：16GB 单卡跑不了 Moshi 7B BF16。** 需要 24GB（RTX 3090/4090 级别）
或有效的 Q8 量化（bitsandbytes Q8 有 bug，candle Q8 被编译问题卡）。

### Rust 编译瓶颈

Moshi Rust 后端锁 candle 0.9.1（2024 年版本），与我们的环境有两处不兼容：
1. **CUDA 13.2**：cudarc 0.16.2 版本检查硬编码（已 patch）
2. **sm_120 Blackwell**：candle-kernels 0.9.1 的 .cu 文件不覆盖 sm_120
   （需要 candle 0.10+ 的 sm_120 kernel）
3. **sentencepiece-sys**：GCC 15 对 C++ 声明匹配更严格（旧版 sentencepiece 源码不兼容）

## 文件位置

- Moshi 仓库：`/workspaces/gui_agent/moshi/`（depth-1 clone）
- Rust 后端：`moshi/rust/`（moshi-backend + moshi-core + moshi-cli + moshi-server）
- cudarc patch：`~/.cargo/registry/.../cudarc-0.16.2/build.rs`（13.2→12.8 映射）
- 调研文档：`docs/realtime-voice-models.md`（S2S 模型全景）

## 解锁条件

| 条件 | 状态 | 影响 |
|---|---|---|
| 24GB+ 显存 | ❌ 当前 16GB | 解锁 Python BF16 GPU |
| bitsandbytes Q8 修复 | ❌ 上游 bug | 解锁 Python Q8（7GB 权重适配 16GB）|
| sentencepiece-sys GCC 15 修复 | ❌ 待 patch | 解锁 Rust CPU Q8 |
| candle 0.9.1 → 0.10+ 升级 | ❌ Moshi 锁版本 | 解锁 Rust CUDA sm_120 |

## 后续建议

1. **搁置 Moshi**——16GB 硬件不支持，投入产出不成比例。
2. **专注级联管道优化**（路线 B）——当前 ASR + Stage2 + TTS 架构已验证，
   补 Kokoro TTS（~100ms）后总延迟 ~400ms，够用。
3. **等硬件升级**——换 24GB+ 卡后，Python BF16 直接能跑，无需改代码。
4. **关注 Hibiki**——Kyutai 同期的语音翻译模型，可能更轻量。

相关：[[realtime-voice-models]] [[ai-secretary-north-star]]
