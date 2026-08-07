# dp-models 设计：LocalAI 接管模型部署 + Rust 保留实时语音

> 状态：🟡 **设计**（2026-08-07 第二轮 pivot）。结论：
> **模型部署由 LocalAI（Go 服务）统一接管；VAD + 流式 ASR 留在 Rust（dp-models crate）；
> Audio Aura 不再直接依赖 sherpa-onnx。** 实现时以代码为准。

## 一、最终架构

```
[audio-aura (Rust) 业务进程]
   ├── Stage1 帧循环 (20ms):
   │     VAD (Silero)  +  流式 ASR (Zipformer)   ← crates/dp-models (Rust, sherpa-onnx 保留于此)
   │     └── batch ASR (SenseVoice / Qwen3-ASR)  ← LocalAI (OpenAI /v1/audio/transcriptions)
   ├── Stage2/3: LLM (整流/路由)                 ← LocalAI (/v1/chat/completions)
   └── Provider 抽象 + Http* 客户端               ← crates/dp-models (已有)
            │  OpenAI 兼容 HTTP
            ▼
[LocalAI (Go) 模型服务]   ← 引入 Go 依赖, 模型部署由它接管
   ├── sherpa-onnx 后端      (batch ASR: SenseVoice/Whisper/Qwen3-ASR; TTS)
   ├── transformers / qwen-asr / vllm 后端 (Transformers 生态)
   └── llama-cpp 后端        (GGUF)
```

### 职责边界

| 组件 | 职责 | 关键技术 |
|---|---|---|
| **crates/dp-models**（Rust） | ① **VAD + 流式 ASR**（从 aura-asr 迁移）② Provider trait（Asr/Llm/Tts/Vlm）③ Http* 客户端 ④ build 工厂 | sherpa-onnx（仅 VAD/流式用）、reqwest |
| **audio-aura**（Rust） | Stage1 帧循环组装、Stage2/3 业务逻辑 | **不再依赖 sherpa-onnx** |
| **LocalAI**（Go） | 模型部署层：batch ASR / LLM / TTS / 未来多模态；加载/卸载/生命周期/VRAM | Go，引入项目（go.mod） |

### 为什么这样切（决策依据）

1. **VAD + 流式留 Rust**：与 Stage1 的 20ms 帧循环深度耦合，走 HTTP 有延迟/吞吐顾虑；sherpa-onnx
   的 OnlineRecognizer/VAD 是成熟 Rust 资产，迁移到 dp-models 后 Audio Aura 与 sherpa 解耦。
2. **batch ASR + LLM 交给 LocalAI**：它们是"一个请求一个结果"的形态，天然适合服务化；LocalAI
   的 sherpa-onnx 后端（OfflineRecognizer/OfflineTts）和 transformers/vllm/llama-cpp 后端全覆盖
   我们的三生态需求。
3. **LocalAI 而非自研 Python 服务**：LocalAI 已实现我们需要的全部管理机制（模型注册表、声明式
   配置、加载/卸载/VRAM、健康自愈、多后端），避免重复造轮子。

## 二、迁移影响

### crates/dp-models（Rust）——从"纯客户端"变为"客户端 + 实时语音"

- **迁入**：`aura-asr` 的 VAD（`OnnxVad`/`EnergyVad`）+ 流式 ASR（`OnlineAsr`，Zipformer）→
  dp-models 新模块（如 `speech/`）。sherpa-onnx 依赖迁入 dp-models。
- **保留**：`AsrProvider/LlmProvider/TtsProvider/VlmProvider` trait、Http* 客户端、build 工厂。
- **新增**：流式/实时语音的 trait（供 Stage1 帧循环用）。

### audio-aura —— 删除 sherpa-onnx 依赖

- Stage1Executor 改造：VAD/流式从 dp-models 拿；batch ASR 改调 LocalAI（HttpAsr）。
- `aura-asr` crate 变薄：OnnxAsr/OnlineAsr/OnnxVad 迁走后，保留 Asr trait re-export + 管线组装。
- `aura-core` 的 `Calibrator`（mistral.rs）→ LocalAI（HttpLlm）；Stage2CalibratorImpl 保留
  （提示词/热词/上下文），内部 LLM 调用走 LocalAI。

### 引入 Go + LocalAI

- 环境：安装 Go（已完成：go1.26）。
- 项目：LocalAI 源码进入仓库（或作为子模块/vendored）；文档化启动方式（models 目录声明 +
  `local-ai run`）。
- 模型声明：LocalAI 的 `models/*.yaml`（name/backend/parameters.model）即我们的 ModelSpec 落地。

## 三、分步实施

1. **dp-models 迁入 VAD + 流式**：从 aura-asr 移动 OnnxVad/OnlineAsr 到 dp-models；
   aura-asr 改为 re-export/委托；确认 Stage1 帧循环不受影响（回归测试）。
2. **搭建 LocalAI**：安装 Go（✓）、LocalAI 可构建（✓ 已验证）、配置 batch ASR 模型
   （sherpa-onnx 后端 SenseVoice）+ LLM 模型（llama-cpp），业务侧 Http* 连通验证。
3. **audio-aura 切换 batch ASR 到 LocalAI**：Stage1Executor 的 batch 路径改调服务；
   删除 aura-asr 的 OnnxAsr（或保留为 fallback）。
4. **Stage2/3 切换 LLM 到 LocalAI**：Calibrator → HttpLlm；删除 mistral.rs 依赖。
5. **audio-aura 删除 sherpa-onnx 依赖**：确认 VAD/流式走 dp-models 后清理。
6. **（可选）TTS/未来模型**：LocalAI sherpa-onnx TTS 后端接入。

每步独立可验证（编译 + 回归 + 端到端）。

## 四、已决策记录（2026-08-07）

- ONNX 语音（VAD/流式）留 Rust（dp-models）；batch ASR 走 LocalAI。
- 模型部署层用 LocalAI（Go），不自研 Python 服务。
- Transformers/GGUF 生态由 LocalAI 的后端体系覆盖（transformers/vllm/llama-cpp）。
- **LocalAI 引入方式**：在 `apps/dp-models` 下新增 Go 项目（LocalAI 源码），独立进程形态运行。
- **batch ASR 双轨**：OnnxAsr 迁入 dp-models 保留为 fallback（删除为时过早，先放着）。
- **sherpa-onnx 依赖**：dp-models 直接依赖 sherpa-onnx crate（feature-gated，默认不开启——
  本模块的目的就是把重依赖从 aura 进程隔离）。
- **batch 调用形态**：先按现状每段一个 HTTP 请求（HttpAsr 同步调用）；延迟问题后续实测再定。

## 五、开放问题

（已全部决策，无遗留。Stage1 的 batch 延迟实测为后续验证项。）
