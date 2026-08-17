# Stage1 / Stage2：能力与流程（as-built 2026-08-17）

> 代码为准。本文是语音识别两个 stage 的权威梳理：各自的职责边界、内部流程、衔接契约。
> 代码入口：Stage1 = `crates/aura-asr/src/executor.rs`，Stage2 = `crates/aura-core/src/calibrator.rs`，
> 组装 = `crates/aura-core/src/composer.rs`，daemon = `apps/audio-aura/src/main.rs`。

## 总览：数据流

```
omni-scout /audio (TCP)
   │  ingest 线程 aura-stage1-ingest
   ▼
AudioRing（10min @16kHz mono）
   │  consume loop 每 tick 取 512 样本（32ms Silero 窗）
   ▼
┌──────────────────── Stage1（音频 → 文本）────────────────────┐
│ ① 流式 Zipformer partial（~0.5s 节流，变化才发）→ Interim      │
│    └ 勤快 Stage2：每 1s 把 partial 文本包成 Batch 动作送校准    │
│ ② Silero VAD SOS/EOS → SegmentMerger 碎片合并                 │
│    ├ gap < merge_gap：吸收 + 重跑批式 → Batch（临时）          │
│    └ gap ≥ merge_gap / 静默超时：定稿   → MergeBatch（权威）   │
└──────────────────────────────────────────────────────────────┘
   │  mpsc（Stage2 独立 worker 线程 aura-stage2）
   ▼
┌──────────────────── Stage2（文本 → 纠偏文本）─────────────────┐
│ Batch      → calibrate_provisional（不写 ContextWindow）      │
│ MergeBatch → calibrate           （写入 ContextWindow）       │
│ LLM 整流：加标点 / 修同音错字 / 英文规范 / 专有名词修正          │
└──────────────────────────────────────────────────────────────┘
   ▼
daemon on_turn 回调 → SSE 数据面 /api/asr_stream → UI
   └ Final 同时落盘（recordings WAV + turns jsonl）+ Stage3 规则加词
```

## Stage1：音频 → 文本（ONNX 语音前端）

**位置**：`crates/aura-asr/src/executor.rs`（`OnnxStage1Executor`）。ONNX 语音栈
（Silero VAD / Zipformer 流式 / SenseVoice·Whisper·Qwen3-ASR 批式）封装在
`dp-models::onnx`，aura-asr 不直接依赖 sherpa-onnx。

**能力**：音频采集、VAD 切段、**两遍识别**（two-pass：流式 partial + 批式 final）、
碎片合并。不做文件 IO、不跑 Stage2——只发事件。

### 流程

1. **采集**：ingest 线程从 scout 收 32ms 窗写入 `AudioRing`（10min 容量）；断流 >2s
   且有 partial 时喂合成静音逼 VAD 发 EOS（executor.rs:490）。
2. **流式通道**：每 ~15 窗（≈0.5s）解码一次 Zipformer，文本有变化才发 `Interim`
   （seq = 定稿序号+1，供 UI 归组）。同时每 1s 把 partial 文本包装成 `Batch` 动作送
   Stage2——partial 已是文本，无需批式 ASR，只有一次廉价 LLM 调用
   （`STREAM_CALIBRATE_INTERVAL`，executor.rs:41）。
3. **VAD 切段与合并**（`SegmentMerger`，executor.rs:251-373）：
   - `min_silence`（1s）保持小 → 停顿一过立即切，响应快，代价是长句变碎片；
   - 碎片间隔静音 < `merge_gap`（aura.yaml 当前 2.5s，内置默认 5s）→ 拼回同一
     utterance，**批式 ASR 重跑合并后 PCM**，同一 seq 就地更新（`Batch`，临时）；
   - 间隔 ≥ merge_gap 或后续静默满 merge_gap（`check_settle`；说话中抑制超时）→
     定稿 `MergeBatch`（权威）。
   - 有效合并窗口 = (min_silence, merge_gap) ≈ 1~2.5s——"什么算一句话"的旋钮。
4. **定稿转写** `transcribe_final`（executor.rs:404）：先 finalize 流式会话得到热词偏置的
   `streaming_text`，再批式识别合并 PCM 得权威 `raw_text`，然后**换新的流式会话**
   （reset 会泄漏编码器上下文）。流式会话横跨整个合并 utterance，不按 VAD EOS 重置。

### 输出契约（aura-asr/src/lib.rs:63-92）

| 事件 | 含义 | 去向 |
|---|---|---|
| `Interim` | 流式 partial | 直通 UI，**不是** Stage2 输入 |
| `Action(Batch)` | 停顿 ≥min_silence 的临时批式结果 / 1s 周期的 partial 纠偏 | Stage2 临时校准 |
| `Action(MergeBatch)` | 停顿 ≥merge_gap 的定稿合并段落 | Stage2 权威校准 |

`Utterance` 载荷：`seq / raw_text（权威）/ streaming_text（热词偏置，回退）/
duration_ms / at_s / pcm`。Stage2 取 `route_text()`（批式为空时回退流式）。

### 后端与部署形态

- 批式 ASR 可选：SenseVoice（默认）/ Whisper large-v3-turbo / Qwen3-ASR 1.7B int8；
  local sherpa ONNX（provider cpu|cuda）或 remote HTTP（`asr_kind=remote`）。
- **流式 + VAD 恒为本地 CPU**（实时性要求）；批式可上 CUDA。
- 当前 `aura.yaml` 实跑：`asr_backend: qwen3-asr` + `asr_kind: remote`
  （http://127.0.0.1:8000）。

## Stage2：文本 → 纠偏文本（LLM 整流）

**位置**：`crates/aura-core/src/calibrator.rs`（`Stage2CalibratorImpl`）+
`prompt.rs`（PromptBuilder）+ `context.rs`（ContextWindow）+ `decision.rs`（Decision）。

**能力**：对 Stage1 文本做 LLM 纠偏——加标点、修同音/谐音错字、英文前后加空格、
专有名词修正。跑在独立 `aura-stage2` worker 线程（composer.rs:62），LLM 1-2s 的
耗时不会卡住流式 partial（N+1 的 Interim 可以先于 N 的 Final 到达）。

### 流程（`calibrate_inner`，calibrator.rs:88）

1. 取 `route_text()`，用 `PromptBuilder` 构造 prompt：
   - system：角色一句话（"语音文字纠偏助手…只修改确信是错误的部分，不确定就保留
     原文"）+ OUTPUT 四条规则（加标点 / 修错字 / 英文规范 / 专有名词）；
   - **用户纠正对**（POST /api/correct 写入，环形 20 条）作为权威示例，优先级最高；
   - user：原文包 `<raw_transcript>` XML 信封防注入（prompt.rs:206）。
2. LLM 调用：local mistral.rs GGUF（当前 `qwen2.5-3b-instruct-q4_k_m`）或 remote
   OpenAI 兼容 `HttpLlm`；**失败回退原文**。输出纯文本，无 JSON 解析。
3. `commit` 区分：`Batch` → 不写 ContextWindow；`MergeBatch` → 写入（滚动 5 对
   raw→calibrated）。

### 当前被关闭的通道（小模型能力妥协，恢复条件见注释）

| 通道 | 状态 | 恢复条件 |
|---|---|---|
| ContextWindow 注入 | ❌ 禁用（calibrator.rs:91） | ≥7B 模型可靠遵循"不要复读" |
| Stage2 prompt 热词块 | ❌ 注释（prompt.rs:170） | —（热词只烘进 Stage1 流式 recognizer） |
| 默认 few-shot 示例 | ❌ 注释（prompt.rs:34） | — |
| 双通道对照 `streaming_ref` | ❌ 禁用（calibrator.rs:98） | 模型能调和双转写时 |
| OUTPUT"去口语"规则 | ❌ 注释（prompt.rs:68） | — |

`Decision` 的 `intent/reply/task` 字段目前是摆设（恒 `"chat"`/空），`parse_decision`
的 JSON 解析路径是历史遗留（JSON 输出时代）。

### 反馈闭环

- **Stage3 → Stage2**：daemon 内规则触发器（main.rs:550）从定稿文本提取大写专名
  （拒绝 "APIdocker" 类拼接伪影）写入共享热词 store，Stage2 每次校准读最新值。
  热词运行时下沉到 ASR recognizer（重建烘焙）是 **M5 待办**。
- **用户 → Stage2**：`POST /api/correct` 推入 corrections 环（cap 20）。
- 种子热词双层注入：boot 时烘进流式 recognizer（beam bias）+ 预载 Stage2 共享 store。

## 线程模型与事件时序

| 线程 | 职责 |
|---|---|
| `aura-stage1-ingest` | scout TCP → AudioRing |
| `aura-pipeline`（std 线程） | Stage1 consume loop（阻塞式睡眠轮询，异步化是待办） |
| `aura-stage2` | LLM 校准 worker（mpsc 收 Stage1Action） |
| `aura-socket`（tokio 多线程） | axum HTTP/SSE：数据面 `/api/asr_stream` + 控制面 `/api/stream` |

| 事件 | seq 语义 | UI 表现 |
|---|---|---|
| `Interim` | idx+1（进行中） | 打字机式实时文本 |
| `CalibratedInterim` | 同上，就地更新 | 合并/碎片后刷新同一句 |
| `Final` | idx（定稿） | 定稿 + 落盘 + Stage3 |

识别事件走**数据面**（broadcast 直推，不节流）；设置变更（scout 开关/热词/纠正）
走**控制面**（version 计数器 → 节流 ping → 客户端重拉 `/api/state` 快照）。
详见 `client-state-sync.md`。
