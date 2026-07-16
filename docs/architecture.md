# audio-aura 架构（as-built）

> 现状权威文档（2026-07-15，脊柱重构后）。与代码为准。北极星见 [[ai-secretary-north-star]]。

## 定位：语音助手前端 + 中间守护进程

audio-aura 是**系统级 AI 秘书**的语音功能层，在整盘棋里的位置：

```
        geek-familiar  (Rust 桌面宠物悬浮窗 = 秘书 UI + agent 调度)
              │  TCP/unix socket
        ▼
     aura-daemon   ← 本仓库：语音助手前端 + 三阶段提交管线
              │  HTTP (omni-scout /audio)
        ▼
     omni-scout  (录音源：PipeWire / mock-audio)
```

audio-aura 用 **AI-agent 手段把 ASR 准确率榨到极致**（别人只做 1+2 阶段，我们加第三阶段：可选的带工具元 agent）。它**不是**整个秘书——视觉/操作在 visual-rover，宠物形态/调度在 geek-familiar；audio-aura 只管"听准 → 整流 → 路由 → 反馈"。

## 三阶段提交

| 阶段 | 职责 | crate | 抽象 |
|---|---|---|---|
| **Stage1** | 录音 → VAD → 两阶段 ASR（流式 Zipformer 偏置 partial + 批式 SenseVoice 权威 final） | `aura-asr` | `Stage1Executor`（发 `Stage1Event::{Interim,Final(Utterance)}`） |
| **Stage2** | 口语整流 + 意图路由（Qwen3-1.7B via mistral.rs，GPU）+ (raw,calibrated) 对上下文 | `aura-dcl` | `Stage2Calibrator`（`calibrate(&Utterance)->Decision`） |
| **Stage3** | 可选的带工具元 agent：热词整理 / 动态微调 / 上下文归纳 / 长期记忆 | `aura-agent` | 能力 trait（`HotwordManager`/`FineTuner`/`ContextSummarizer`/`MemoryStore`）+ `Tool` |

**关键原则**：Stage3 的**能力**在 `aura-agent`，**调度**（何时微调、用哪些标注）在 geek-familiar 这个秘书 agent（经 daemon socket 发起）。本轮 daemon 内挂一个进程内规则触发器跑通闭环 demo，geek-familiar 接入后替换它。

## crate 拓扑（依赖自下而上，无环）

```
aura-asr   (Stage1 叶子)   Stage1Executor + Utterance/Stage1Event + onnx(VAD/ASR)
aura-tts   (占位叶子)       Tts trait + NoopTts   ← 本轮占位，真模型(Kokoro/Piper)后续
aura-dcl   (Stage2) ► asr   Stage2Calibrator + RouterEngine(ContextWindow/PromptBuilder)
aura-core  (组装车间) ► asr+dcl   Pipeline + TurnEvent   ← legacy main/ingest/pipeline 待迁
aura-agent (顶层)           能力 trait + Tool + AddHotwordTool（只实现 Hotword，无调度）
daemon     (二进制) ► core+agent   Pipeline + Stage3 规则触发器 + socket 骨架
```

**数据契约**：`Utterance`/`Stage1Event` 在 `aura-asr`（不 gate `onnx`，故 `aura-dcl` 不被迫拉 sherpa）；`Decision` 在 `aura-dcl`；aura-agent 同时见二者。

**Stage3→Stage2 反馈通道**：共享 `Arc<Mutex<Vec<String>>>` 热词 store——Stage3 加词 → Stage2 下次 `calibrate` 读最新。闭环已验证（见下）。
**Stage3→Stage1 反馈**：暂不可行——sherpa 在 `OnlineRecognizer` 创建时烘焙热词，运行时不能动态改（需重建 recognizer / per-stream 热词，TODO）。

## 破环（迁移要点）

重构前 `stage12_live` 在 `aura-asr/examples`，且 `aura-asr` 经 `bench-live` 依赖 `aura-dcl`（用 RouterEngine）。要让 `aura-dcl ► aura-asr`（Stage2 消费 Stage1 的 `Utterance`）就会成环。解法：把 `stage12_live` 移到 `aura-core/examples/`（薄壳），`aura-asr` 卸掉 dcl 依赖变真叶子 → `aura-dcl ► aura-asr` 单向。

## 双运行时（ONNX + HF）

进程内两个隔离运行时，各管各的、只通过"文本"交互（见 `runtime-selection.md`）：
- **ONNX 侧**（`sherpa-onnx` 官方 crate，单一 onnxruntime）：VAD(Silero) + ASR(SenseVoice 批式 + Zipformer 流式)。
- **HF 侧**（`mistral.rs`/candle，GPU sm_120）：Qwen3-1.7B 整流+路由（Stage2）。

## 运行

```bash
# 1. 起 omni-scout 录音源（mock 或真麦）
omni-scout --port 7879 --mock-audio <wav>     # 或真 PipeWire

# 2. 跑 daemon（全管线 + Stage3 闭环 + socket）
cargo daemon -- 127.0.0.1:7879          # = cargo run -p aura-daemon --release --features asr,cuda --
curl http://127.0.0.1:9091/health        # {"status":"ok"}
curl http://127.0.0.1:9091/context       # {"hotwords":[...]}  ← Stage3 填充的热词 store

# 或只跑 S1→S2 行为基准（无 Stage3）
cargo benchlive -- 127.0.0.1:7879        # = aura-core example stage12_live
```

构建期：`NVCC`/`CUDA_PATH`/`CUDA_COMPUTE_CAP` 已在 `.cargo/config.toml [env]`；sherpa `.so` 在工作区 `lib/`（RUNPATH `$ORIGIN`-relative 自定位，零 ldconfig，见 `ldconfig.md`）。

## 已验证（2026-07-15）

- 26 单测全绿（asr 9 / dcl 11 / tts 1 / agent 5）。
- 行为对齐：Pipeline 版输出 == 重构前 `stage12_live`（rost→Rust、蛇声→蛇身、计分起→计分器、readdme→README、B位→Bevy；路由 ~0.5–1.4s GPU）。
- Stage3 闭环：热词 store 初始空 → Stage3 规则触发器加 Rust/Bevy → `GET /context` 实时显示。
- ASR 层热词（同音词类）生效：蛇身 7:舌身 0（见 `stage2-optimization.md` §2.1）。

## 未完成 / 后续

见 `roadmap.md`。
