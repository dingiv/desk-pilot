# 模型运行时选型与双运行时架构

> 2026-07-14 调研与讨论结论。这是 audio-aura 进程内**所有模型加载、推理、生命周期管理**的架构依据。

---

## 一、背景：为什么混用两个生态是别无选择

audio-aura 是语音秘书，需要两类模型：

1. **语音模型**（ASR / VAD / TTS）—— 必须实时、低延迟、成熟复用
2. **文本大模型**（整流 / 意图路由 / 写作 / 记忆）—— 需要长上下文、高质量推理

这两类模型落在**两个互不兼容的推理生态**里，无法统一到一个引擎：

| 生态 | 模型格式 | 引擎 | 擅长 |
|---|---|---|---|
| **ONNX** | `.onnx` | ort / sherpa-onnx | 语音(ASR/VAD/TTS)、跨平台部署、小模型 |
| **HF 原生** | Safetensors / GGUF | candle / mistral.rs / llama.cpp / vllm | 大语言模型(LLM)、自回归+KV-cache |

### 为什么不能"全站一边"

- **LLM 走不了 ONNX**：Qwen/Llama 的自回归 + KV-cache + paged-attention，ONNX 计算图表达不了/极差。Qwen 没有 ONNX 推理路径 → Stage2/3 **必须** candle/mistral.rs。
- **语音走不了 candle**：SenseVoice/Silero/Whisper/Piper/Kokoro 的**主流发布格式是 ONNX**，没有 Safetensors/GGUF 版 → Stage1 **必须** ort/sherpa-onnx。
- 从头自研（用 candle 重写语音模型）代价过大，且放弃社区成熟成果。

**结论：进程内必须同时容纳两个生态的运行时，各管各的。**

### ONNX 生态 2026 现状（站队判断）
- 语音(ASR/VAD/TTS)：ONNX **仍是主流发布格式**，sherpa-onnx 生态成熟。
- 通用 LLM：ONNX **已败退**（LLM 部署全站 GGUF/Safetensors）。
- ONNX 是"部署格式"（语音/小模型/跨平台），HF 原生是"大模型推理格式"。两者非对手，是分工。

---

## 二、两个生态里我们选谁

### ONNX 侧：**sherpa-onnx 官方 crate**（统一）
- 选 [k2-fsa/sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) 官方 Rust crate（crates.io `sherpa-onnx`，v1.13.x，活跃维护）。
- **一站式**：ASR(SenseVoice) + VAD(Silero) + TTS(Kokoro/Piper) 全覆盖，自带 featurizer/decoder，省去手写脏活。
- **进程内只有一份 onnxruntime**（sherpa 自带），杜绝冲突。

> **否决项（已踩坑）：**
> - ❌ `sherpa-rs`（thewh1teagle）—— **已 archive，停止维护**。其 SileroVadEngine 在 Blackwell+CUDA13.2 上会卡死整机；且自带 onnxruntime 与 ort 的 onnxruntime 符号冲突。
> - ❌ `ort` + `sherpa-rs` 混用 —— 两份 onnxruntime 同进程符号冲突，死锁。
> - ❌ 纯 `ort` 跑 SenseVoice —— 要自写 mel FBank featurizer（几百行易错），成本高。

### HF 侧：**mistral.rs (candle)**（已在用）
- Stage2/3 的 Qwen LLM 走 `mistral.rs`（candle 后端，GGUF，GPU sm_120 已验证 ~400ms）。
- `audio-aura-router` crate 的 `RouterEngine` 已是这条路径。

### 备选（若 sherpa-onnx 官方 crate 的 Silero 仍卡）
- ASR 换 `qwen3-asr`（candle，纯 Rust，与 mistral.rs 同栈）—— 但放弃 SenseVoice，且不含 VAD/TTS。
- VAD 换纯 Rust（webrtc-vad）或 tract-based（silero-vad-rs，纯 Rust ONNX 引擎，不碰 onnxruntime）。
- 这是退路，首选仍是 sherpa-onnx 官方 crate。

---

## 三、架构：双运行时管理器（Dual-Runtime Manager）

进程内两个**隔离的运行时管理器**，各自负责本生态模型的加载/预热/推理/卸载，互不干扰。

```
                       audio-aura 进程
┌──────────────────────────────────────────────────────────┐
│                                                           │
│   ┌────────────────────────┐   ┌────────────────────────┐ │
│   │  ONNX 运行时管理器       │   │  HF 运行时管理器        │ │
│   │  (sherpa-onnx)          │   │  (mistral.rs / candle) │ │
│   │                         │   │                        │ │
│   │  · 单一 onnxruntime 实例 │   │  · 单一 candle 实例     │ │
│   │  · VAD  (Silero)        │   │  · LLM  (Qwen3-1.7B)   │ │
│   │  · ASR  (SenseVoice)   │   │  · (未来: Writer 大模型) │ │
│   │  · TTS  (Kokoro, M3)   │   │                        │ │
│   └────────────┬───────────┘   └────────────┬───────────┘ │
│                │                            │              │
│          Stage1 管线                   Stage2/3 管线       │
│   (VAD→ASR, 实时, onnx)           (整流/路由/写作, HF)     │
│                │                            │              │
│                └──────── 文本 ──────────────┘              │
│                  (两管理器之间只通过文本交互)                │
└──────────────────────────────────────────────────────────┘
```

### 核心原则

1. **每个生态只有一份引擎实例**
   - ONNX 侧：一个 onnxruntime（sherpa 自带，不再引入 ort）。
   - HF 侧：一个 candle（mistral.rs）。
   - **绝不**同进程加载第二份 onnxruntime（这是 sherpa-rs + ort 冲突的根源）。

2. **运行时管理器负责全生命周期**
   - 常驻加载（不是每次推理 new 模型）、预热（首推理暖机）、复用、显存预算、错误恢复、卸载。
   - 统一接口：`load() / warm() / infer() / unload()`。

3. **管理器之间只通过"文本"交互**
   - Stage1（ONNX）输出识别文本 → 交给 Stage2（HF）整流/路由。
   - 不共享模型对象、不共享显存、不共享引擎。
   - 数据边界清晰，两生态解耦。

4. **管线在管理器内、跨管理器靠文本**
   - Stage1 管线（VAD→ASR）全部在 ONNX 管理器内完成。
   - Stage2/3（整流/路由/写作）全部在 HF 管理器内完成。

---

## 四、落地形态（Rust crate 规划）

```
audio-aura-asr (Stage1, ONNX 侧)
  ├── OnnxRuntimeManager   ← sherpa-onnx 单实例，管 VAD/ASR/TTS 的加载与推理
  ├── vad  (Silero via sherpa)
  ├── asr  (SenseVoice via sherpa)
  └── tts  (Kokoro via sherpa, M3)

audio-aura-router (Stage2/3, HF 侧)
  └── RouterEngine         ← mistral.rs/candle 单实例 (已在用)

audio-aura-core (daemon, 编排)
  └── 在两个管理器之间用"文本"串起 Stage1→2→3 管线
```

> 现状：audio-aura-asr 目前混用了 sherpa-rs(archive, 有bug) + ort(冲突源)。**动工第一步就是清理**：移除 sherpa-rs 与 ort，统一换成 sherpa-onnx 官方 crate，由 OnnxRuntimeManager 统一持有。

---

## 五、动工前必须验证的两件事

1. **sherpa-onnx 官方 crate 的 Silero VAD 在 Blackwell+CUDA13.2 不卡死**（archive 的 sherpa-rs 就是卡在这）。
2. **sherpa-onnx(onnxruntime) 与 mistral.rs(candle) 同进程共存**（两套不同引擎同进程，是否冲突——这与 onnxruntime 之间的冲突是不同问题，需单独验证）。

两件都通过 → 双运行时架构用 `sherpa-onnx` + `mistral.rs` 落地。

---

## 六、决策记录

- **不抛弃 ONNX**：语音领域 ONNX 仍是主流，sherpa-onnx 成熟，自研代价过大。
- **不强求统一**：LLM 必须 HF 生态，语音必须 ONNX 生态，混用是别无选择。
- **隔离而非混用引擎**：两个管理器各持一份引擎实例，**绝不**在同生态内引入第二份引擎（onnxruntime 冲突的教训）。
- **ONNX 侧统一 sherpa-onnx**：弃用 archive 的 sherpa-rs 与冲突的 ort，统一官方 crate。
- **HF 侧统一 mistral.rs**：已在用，验证可用。

---

## 参考资料
- [k2-fsa/sherpa-onnx (GitHub)](https://github.com/k2-fsa/sherpa-onnx) · [sherpa-onnx crate (docs.rs)](https://docs.rs/sherpa-onnx) · [sherpa-onnx (crates.io)](https://crates.io/crates/sherpa-onnx)
- [thewh1teagle/sherpa-rs (已 archive)](https://github.com/thewh1teagle/sherpa-rs) — 弃用，原因见上
- [qwen3-asr (备选)](https://crates.io/crates/qwen3-asr) · [second-state/qwen3_asr_rs](https://github.com/second-state/qwen3_asr_rs)
- [whisper-rs](https://crates.io/crates/whisper-rs) · whisper-cpp-plus（退路）
- [SenseVoice (FunAudioLLM)](https://github.com/FunAudioLLM/SenseVoice) · [Silero VAD](https://github.com/snakerspace/silero-vad)
- 相关文档：[[stage1-2-problems]] · [[adaptive-learning]] · [[livekit-port-notes]]
