# audio-aura 架构（as-built）

> 现状权威文档（**2026-07-17**，Stage2 移线程 + 配置/日志/存储三基建落地后）。与代码为准。北极星见 [[ai-secretary-north-star]]。

## 定位：语音助手前端 + 中间守护进程

audio-aura 是**系统级 AI 秘书**（desk-pilot）的语音功能层，在整盘棋里的位置：

```
        geek-familiar  (Rust 桌面精灵悬浮窗 = 秘书 UI + agent 调度)
              │  TCP/unix socket
        ▼
     aura-daemon   ← 本子系统：语音助手前端 + 三阶段提交管线
              │  HTTP (omni-scout /audio)
        ▼
     omni-scout  (录音源：PipeWire / mock-audio)
```

audio-aura 用 **AI-agent 手段把 ASR 准确率榨到极致**（别人只做 1+2 阶段，我们加第三阶段：可选的带工具元 agent）。它**不是**整个秘书——视觉/操作在 visual-rover，精灵形态/调度在 geek-familiar；audio-aura 只管"听准 → 整流 → 路由 → 反馈"。

## 三阶段提交（模块化组织的主轴）

| 阶段 | 职责 | crate | 抽象 |
|---|---|---|---|
| **Stage1** | 录音 → Silero VAD → 两遍 ASR（流式 Zipformer 热词偏置 partial + 批式 SenseVoice 权威 final） | `aura-asr` | `Stage1Executor`（发 `Stage1Event::{Interim{seq,..},Final(Utterance)}`） |
| **Stage2** | 口语整流 + 意图路由（Qwen3-1.7B via mistral.rs，GPU）；输入**双通道文本**（批式权威 + 流式补段头/段尾）+ (raw,calibrated) 对上下文 + 共享热词 | `aura-dcl` | `Stage2Calibrator`（`calibrate(&Utterance)->Decision`） |
| **Stage3** | 可选的带工具元 agent：热词整理 / 动态微调 / 上下文归纳 / 长期记忆 | `aura-agent` | 能力 trait（`HotwordManager`/`FineTuner`/`ContextSummarizer`/`MemoryStore`）+ `Tool` |

**关键原则**：Stage3 的**能力**在 `aura-agent`，**调度**在 geek-familiar（经 daemon socket）。当前 daemon 内挂进程内规则触发器代位：从整流文本提取大写拉丁词加热词，`looks_like_concat`（大写-大写-小写三连 = 拼接缝）拦"APIdocker"类垃圾词；geek-familiar 接入后替换之。

## crate 拓扑（依赖自下而上，无环）

```
aura-asr   (Stage1 叶子)     Stage1Executor + Utterance/Stage1Event + onnx(VAD/ASR)
aura-store (持久化，独立)     hub::Storage 总管 = AudioArchive + TurnLog + recent 环；wav.rs；遗留 sqlite 4 表
aura-tts   (占位叶子)         Tts trait + NoopTts   ← 真模型(Kokoro/Piper)后续
aura-dcl   (Stage2) ► asr     Stage2Calibrator + RouterEngine(ContextWindow/PromptBuilder)
aura-core  (组装车间) ► asr+dcl   Pipeline + TurnEvent   ← legacy main/ingest/pipeline 待迁(M4)
aura-agent (顶层)             能力 trait + Tool + AddHotwordTool（只实现 Hotword，无调度）
daemon     (apps/audio-aura) ► core+agent+store   Pipeline + Stage3 规则触发器 + socket
```

**数据契约**：`Utterance`/`Stage1Event` 在 `aura-asr`（不 gate `onnx`）；`Decision` 在 `aura-dcl`；`TurnRecord`/`ClipMeta` 在 `aura-store`。`aura-store` 不依赖任何 aura crate（`aura-asr` 仅 dev-dep 它取 wav 读写做测试）。

**Stage3→Stage2 反馈通道**：共享 `Arc<Mutex<Vec<String>>>` 热词 store——Stage3 加词 → Stage2 下次 `calibrate` 读最新。
**Stage3→Stage1 反馈**：暂不可行——sherpa 在 `OnlineRecognizer` 创建时烘焙热词，动态化需重建 recognizer（M5）。

## 线程模型（2026-07-17 起）

```
aura-pipeline (std 线程)   Stage1 消费循环：ring 取帧 → 流式喂+VAD → Final 丢给 worker，永不等 LLM
aura-stage2  (worker 线程)  逐条 calibrate（~0.5-2.4s）→ 发 TurnEvent::Final
aura-archive (flusher 线程) 每 10s 把未落盘 clip 写 WAV（Weak 引用，archive 释放即自然退出）
aura-stage1-ingest (线程)   scout /audio → AudioRing（断线 2s 重连）
tokio (主线程)              axum socket + SSE
```

**事件契约**：Stage2 独立线程后，`Interim(N+1)` 可能先于 `Final(N)` 到达——所有事件携带 `seq`（Interim 的 seq = 进行中话语的预期序号），**消费方一律按 seq 归组，不能按到达顺序**。收益：route 期间流式 partial 不再冻结。

## Stage1 关键实现事实

- **流式解码必须排干**：sherpa `decode()` 只解一步（~320ms/chunk），`decode_and_result` 内部 `while is_ready { decode }`；utterance 收尾走 `finalize_and_result`（`input_finished` + drain 冲出编码器尾块）。曾因单步解码欠账导致 partial 滞后滚雪球 + session 替换时丢积压音频（复盘见 `stage1-2-problems.md` 补章）。`is_ready` 门控同时防住 fresh session 早解码崩溃（无需 warmup hack）。
- **VAD 参数**（讲述式语音调优）：threshold **0.5** / min_silence **1.0s** / min_speech **0.3s** / max_speech **28s**——让自然句间停顿先于 sherpa 的 20s 急切强切模式（0.90/0.1s，词中间腰斩）切分。
- **段头削字的兜底**：VAD 段起点回看余量有限，批式偶发丢段头（如"帮我"）；流式全程连续接收、头尾更全——因此 Stage2 收**双通道文本**（`<raw_transcript>` 权威 + `<streaming_transcript>` 补头尾），由 LLM 合并。
- **热词双层种子**：`SEED_HOTWORDS`（可被 `aura.json` `hotwords` 覆盖）同时烘进流式 recognizer（beam 偏置）与 Stage2 共享库。实测：在表的 Rust 流式全对，不在表的 Docker→DO CAR / GitHub→GUITAR / Kubernetes→KUBERNITIES。

## 存储（aura-store::hub::Storage 总管）

每个 Final 一次 `storage.record_final(FinalTurn)`，同时进三处：

| 件 | 产物 | 说明 |
|---|---|---|
| `AudioArchive` | `data/recordings/<YYYY-MM-DD>/<HHMMSS>_<seq>.wav` | 热层有界回放 + flusher 按期落盘；未落盘永不淘汰 |
| `TurnLog` | `data/turns/<YYYY-MM-DD>.jsonl` | 每 turn 全量 S1+S2 结果（raw/streaming/calibrated/intent/reply/route_ms + wav 路径互链），open-per-append 崩溃安全 |
| recent 环 | 内存最近 100 条 | 撑起 `GET /results` |

## 配置 / 日志

- **配置**：CLI（clap，高频项：`[SCOUT_ADDR] -p/--port --no-stage3`）> `aura.json`（全量：scout_addr/port/stage3/model/hotwords/web_dist/recordings_dir）> 内置默认。**不读业务 env**。文件经 shared FileLoader 命名空间解析：`CONF`（dev: `apps/audio-aura/`，prod: `~/.desk-pilot/`）、`DATA`（dev: `apps/audio-aura/data/`，prod: `~/.desk-pilot/data/`）。`resolve(cli, conf) -> Settings` 是纯函数（可单测）。
- **日志**：`tracing` 门面（库 crate 只用宏）+ `shared::init_tracing()`（二进制 main 首行）：只写 stderr、dev 人读彩色 / release JSON 行、`RUST_LOG` 过滤默认 info（`[stage1]` 周期诊断在 debug 级）。

## socket 接口现状

| 端点 | 状态 |
|---|---|
| `GET /health` `/api/status` | ✅ |
| `GET /api/stream`（SSE：interim/final/status，带 seq） | ✅ |
| `POST /api/control/scout`（aura 侧连接开关） | ✅ |
| `GET /api/audio/:seq`（WAV 回放，热层→磁盘透明） | ✅ |
| `GET /api/recordings`（clip 列表） | ✅ |
| `GET /results`（最近 turns） | ✅ |
| `GET /context`（热词 store） | ✅ |
| `POST /control/speaker`、`POST /annotate` | ⛔ stub（R2/R4） |
| 静态 SPA（web_dist） | ✅（dev 走 Vite） |

## 双运行时（ONNX + HF）

进程内两个隔离运行时，各管各的、只通过"文本"交互（见 `runtime-selection.md`）：
- **ONNX 侧**（`sherpa-onnx` 官方 crate）：VAD(Silero) + ASR(SenseVoice 批式 + Zipformer 流式)。
- **HF 侧**（`mistral.rs`/candle，GPU）：Qwen3-1.7B 整流+路由（Stage2）。

## 运行

```bash
# 1. 起 omni-scout 录音源（配置同样走 CLI > scout.json > 默认）
cargo run -p omni-scout -- --mock-audio hungry_snake.m4a    # 或真 PipeWire（无参）

# 2. 跑 daemon（全管线 + Stage3 规则触发器 + socket）
cargo run -p aura-daemon --features asr,cuda -- 127.0.0.1:7878
curl http://127.0.0.1:9091/health          # {"status":"ok"}
curl http://127.0.0.1:9091/results         # 最近 turns（S1+S2 全量结果）
curl http://127.0.0.1:9091/api/recordings  # 录音列表
RUST_LOG=debug ...                         # 看 stage1 周期诊断
```

构建期：`NVCC`/`CUDA_PATH`/`CUDA_COMPUTE_CAP` 在 `.cargo/config.toml [env]`；sherpa `.so` 在工作区 `lib/`（RUNPATH `$ORIGIN` 自定位，见 `ldconfig.md`）。

## 已验证（2026-07-17）

- **34 测试套件全绿**（含 archive 5 / hub 3 / daemon 3 / prompt 双通道 4）。
- 真麦 `bench/case.txt` 复测：长句（13–17s）流式 partial 全程跟手、final 完整、无强切碎片；ring 尖峰 = route 阻塞期正常缓冲（≈route_ms×16 样本，随后归零）。
- e2e mock：日期落盘 + turns jsonl + `/results` + WAV 逐字节尺寸吻合；热词管线活证（raw `rost语言/B位引擎` → calibrated `Rust语言/Bevy引擎`）。
- 已知残留：英文术语不在热词表时两路皆碎（M5/LoRA 领域）；Stage2 偶发过度删减（提示词已加规则，待更多真麦回归）。

## 未完成 / 后续

见 `roadmap.md`。
