# aura 架构（as-built 2026-08-19）

> 现状权威文档。代码为准。北极星：系统级 AI 秘书（desk-pilot）。
> 三阶段流程细节见 **`stages.md`**；路线图见 **`roadmap.md`**。

## 定位：语音助手前端 + 中间守护进程

aura 是 **desk-pilot** 的语音子系统——"耳朵 + 整流 + 意图"。下接 omni-scout（录音源），
上接 geek-familiar（秘书 UI + agent 调度）。

```
geek-familiar (秘书 UI + agent)
      │  socket
      ▼
aura-daemon (三阶段管线 + HTTP socket)
      │  HTTP (omni-scout /audio) 或 mock-audio
      ▼
omni-scout (PipeWire 真麦 / mock wav)
```

## crate 拓扑（无环）

```
shared          (FileLoader 叶子) — dev/prod 路径解析
dp-models       (通用模型提供库) — ModelProvider 伞形 + 能力 trait (Asr/Llm/Vlm)
  ├─ onnx     本地语音栈 (feature speech): VAD Silero + 流式 Zipformer/x-asr + 批式 SenseVoice
  ├─ mistral  本地 LLM (feature mistral): MistralLlm (mistral.rs GGUF)
  └─ http     远程: HttpAsr/HttpLlm/HttpVlm (OpenAI 兼容, reqwest::blocking)
aura-core       (全栈流程)
  ├─ recognizer Stage1 (feature asr): Silero VAD + 流式 + 批式 + WindowTracker + AudioStore
  ├─ pipeline   组装 (PipelineSpec→assemble 全栈) + Stage2 worker + 识别日志/窗口归档
  ├─ calibrator Stage2CalibratorImpl (窗口状态机) + Stage2Calibrator trait
  ├─ prompt     PromptBuilder (单句指令 + 多段联合 + 双通道信封)
  ├─ lib.rs     Calibrator 封装层 (持有 dp_models::MistralLlm, 附加 prompt 组装)
  ├─ hub        Storage: AudioArchive + TurnLog + recent ring
  ├─ archive / wav / tts / buffer / vad / scout  辅件
aura-agent      (Stage3+SDK) 能力 trait + HotwordManager + rules + view (线协议) + AuraClient SDK
apps/audio-aura (daemon)     config 解析 (CLI/yaml→PipelineSpec) + socket (8 routes) + SSE双面
crates/native                 napi shim (TS via VOICE_LOCAL_ROUTER)
```

**线程模型**：`aura-stage1-ingest`（scout→ring）→ `aura-pipeline`（`Pipeline::spawn` 的
std 线程跑 Stage1 consume loop，Condvar 事件驱动）→ `aura-stage2`（LLM worker，mpsc 收
Batch/WindowEdge）→ `aura-socket`（主线程 tokio，axum SSE）。详见 `stages.md`。

## 三阶段提交

| 阶段       | 职责                                                                  | crate             | 抽象                                                   |
| ---------- | --------------------------------------------------------------------- | ----------------- | ------------------------------------------------------ |
| **Stage1** | 录音→VAD→段级流式+段级batch→窗口定稿（边界范式 VadSegment/VadWindow） | aura-core (`asr`) | Stage1Recognizer（发 StreamFragment/Batch/WindowEdge） |
| **Stage2** | 窗口内多句联合整流，无状态                                            | aura-core         | Stage2Calibrator（calibrate_window / finalize_window） |
| **Stage3** | 可选工具：热词 / 用户纠偏                                             | aura-agent        | HotwordManager + rules 规则触发器                      |

Stage1 的 **VAD + 流式模型本地跑**（实时 partial 要低延迟）；批式 ASR / Stage2 LLM
本地或远程皆可（配置选），方向是**走远程**（见下）。

## dp-models：通用模型提供库

跨子系统（aura/visual-rover）统一 local/remote 模型抽象：

```
dp-models/
├── trait ModelProvider { fn kind() }   # 伞形 marker：标识实现家族 (local-onnx/remote-http/local-mistral)
├── trait AsrProvider  { recognize(pcm, sr) -> text }
├── trait LlmProvider  { complete(system, user) -> text }
├── trait VlmProvider  { complete(system, user, image) -> text }
├── onnx    OnnxAsr / OnnxRuntimeManager  (本地, feature speech)
├── mistral MistralLlm                    (本地, feature mistral)
└── http    HttpAsr / HttpLlm / HttpVlm   (远程, OpenAI 兼容)
```

上层使用者（aura-core / visual-rover）**按需实例化**具体实现，再按能力 trait 取用。
aura-core 里：recognizer `batch_asr: Arc<dyn AsrProvider>`（本地 OnnxAsr / 远程 HttpAsr）；
pipeline `s2: Box<dyn Stage2Calibrator>` 内的 `Arc<dyn LlmProvider>`（本地 `Calibrator`
封装 / 远程 HttpLlm）。

**Stage2 本地 LLM 分层**：模型本体在 `dp_models::MistralLlm`（通用，命名面向能力）；
aura-core 的 `Calibrator` 是**封装层**——持有 `MistralLlm`，附加 Stage2 的 prompt 组装
（`calibrate_blocking`），对外 `audio_aura_core::Calibrator` API 不变。

## 模型选型（本地 / 远程）

| 环节         | 本地                                                   | 远程                                   | 方向     |
| ------------ | ------------------------------------------------------ | -------------------------------------- | -------- |
| VAD (Silero) | ✅ 恒本地                                               | —                                      | 本地     |
| 流式 ASR     | zipformer / **x-asr**                                  | —（要低延迟）                          | 本地     |
| 批式 ASR     | sensevoice（唯一本地；whisper/qwen3-asr 本地模型已删） | OpenAI 兼容 `/v1/audio/transcriptions` | **远程** |
| Stage2 LLM   | mistral.rs Qwen2.5-3B（GGUF）                          | vLLM/SGLang                            | **远程** |

模型文件：dev 在 workspace `assets/models/`，prod 在 `~/.desk-pilot/models/`
（`MODELS` 命名空间，dp-models/aura-core 各声明一份）。当前保留集见
`assets/models/README.md`。

### 双运行时（ONNX + HF，决策记录）

语音与 LLM 落在两个互不兼容的推理生态，无法统一到一个引擎（详见原
`runtime-selection.md`，已并入）：
- **ONNX 生态**：语音（ASR/VAD）主流发布格式是 ONNX，`sherpa-onnx` 官方 crate 一站式
  （VAD Silero + ASR Zipformer/x-asr + 批式 SenseVoice）。
- **HF 生态**：LLM 是自回归 + KV-cache + paged-attention，必须 candle/mistral.rs
  （GGUF/Safetensors），ONNX 表达不了。

**核心原则**：每个生态只有一份引擎实例（**绝不**同进程加载第二份 onnxruntime——sherpa-rs
+ ort 冲突死锁的教训）；两生态只通过文本交互；运行时管理器负责加载/预热/复用/卸载。
已落地：ONNX 栈在 `dp-models::onnx`，LLM 在 `dp-models::mistral`（mistralrs/candle）。

## 配置（aura.yaml，YAML 按模块分层）

当前实跑配置（2026-08-19 重构：命名准确化 + 模块层级，未知键直接拒绝而非静默失效）：

```yaml
scout_addr: "127.0.0.1:7878"   # 音频源（与 ASR 部署无关，local/remote 都吃它）
scout_chunk_ms: 64              # 客户端请求 scout 聚合推流 (ms, HTTP chunk)
bind_addr: "127.0.0.1"
port: 9091
stage3: true
log_level: "debug"              # trace|debug|info|warn|error | EnvFilter; RUST_LOG 优先

asr:                            # Stage1 语音前端
  stream:  { model: x-asr }     # zipformer | x-asr（恒本地）
  backend: remote               # local | remote | disable —— 批式 ASR 部署（流式/VAD 恒本地）
  local:   { model: sensevoice, language: auto, hardware: cpu, threads: 8 }  # 唯一本地批式
  remote:  { endpoint: "http://127.0.0.1:8000" }
  vad:     { threshold: 0.5, min_silence: 1.0, min_speech: 0.3, max_speech: 28.0,
             merge_gap: 3.5, edge_margin: 0.3 }

llm:                            # Stage2
  backend: local                # local (mistral.rs) | remote (vLLM/sglang) | disable
  model: "qwen2.5-3b-instruct-q4_k_m.gguf"   # local: GGUF 文件名; remote: 服务端模型名
  input: both                   # 纠偏输入源: batch(默认) | stream | both

hotwords: ["Rust", "Bevy", "贪吃蛇"]
storage: { retention_days: 7 }  # + recordings_dir 可选
```

要点：`asr.backend`/`llm.backend` 选部署边；`local.model_dir`/`llm.model_dir` 覆盖模型根
（默认 MODELS 命名空间）；VAD 挂 `asr.vad`；`scout_chunk_ms` 是网络层聚合（消费侧照旧
重切 32ms 窗）。**deny_unknown_fields**：拼错/过时键 → parse 失败 → Malformed warn + 默认。

日志分级：**info 只出 final**（每定稿一句一条，含 batch/streaming/calibrated 三层）；
流式 partial 与纠偏碎片为 **debug**（调管线时 `log_level: "debug"`）。
优先级：CLI > aura.yaml > 内置默认（aura.json 仅向下兼容 fallback）。

## 运行

```bash
# dev (assets/models + CARGO_MANIFEST_DIR)
CARGO_MANIFEST_DIR=$(pwd) cargo run -p aura-daemon --features asr,cuda -- 127.0.0.1:7879 -p 9091
```

## 已验证（2026-08 里程碑）

- crate 合并全绿（aura-core 收编 dcl/store + 并入 aura-asr/aura-tts，~3250 行 + 单测）。
- 边界范式（VadSegment/VadWindow）+ 5 事件协议（stream_fragment/batch_segment/batch_window/
  segment_calibration/window_calibration）：aura-core/agent/daemon/swift-ime/geek-familiar 切换完成。
- **x-asr** 流式引擎（`asr.stream.model: x-asr`，2026-08-18）：chunk-480ms 官方导出，
  自带标点；可与 zipformer A/B。tokens.txt 必须官方"token id"两列格式。
- **VAD 门控流式**（2026-08-19）：`detected()` 实时信号门控流式喂帧——空闲零流式 CPU；
  流式与 batch 共享同一段 PCM（一致）。替代了此前的能量门（RMS 代理）。
- dp-models：mistralrs Calibrator 迁入（MistralLlm）+ ModelProvider 伞形 trait；
  aura-core 默认构建不再编译 GPU/LLM 重依赖。
- 模型瘦身：assets/models 从 26GB → 12GB（删孤儿；本地批式只留 sensevoice）。
- 音频持久化：recordings/日期/*.wav + turns/日期.jsonl + 启动索引重建 + 保留期清理。
- 用户纠偏：POST /api/correct → Stage2 纠正段注入 → Web UI 编辑。

## 代码内遗留 TODO（整改时留意）

- `Stage1Recognizer::run`：batch 调用仍在消费线程同步执行（远程 ~3.5s/次会暂停流式）→
  异步化（roadmap R5）；`Stage1Config::new` 内嵌 IO → 拆出（R6）；daemon 静态路径 `BASE`
  硬编码 → FileLoader（R7）。
- dp-models `AsrBackend::Whisper`/`Qwen3Asr` 枚举变体已无构造点（死代码，可清）。

## 未完成

见 `roadmap.md`。
