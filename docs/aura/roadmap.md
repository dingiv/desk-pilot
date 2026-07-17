# 路线图（未完成事宜）

> 2026-07-15 整理，**2026-07-17 更新**。对照 `architecture.md` 现状，按 horizon 排列，每条标「状态 / 价值 / 依赖」。北极星：[[ai-secretary-north-star]]。

## ✅ 已完成

**2026-07-15（立脊柱）**
- [x] 6-crate 拓扑（asr/dcl/tts/core/agent/daemon），依赖无环。
- [x] 三抽象：`Stage1Executor`（吞掉 stage12_live 面条）、`Stage2Calibrator`、`Stage3Agent` 能力层。
- [x] `Pipeline` 组装 + 薄壳 `stage12_live` example；行为对齐通过。
- [x] daemon 二进制 + Stage3 规则触发器闭环（store 空→加 Rust/Bevy→`/context` 显示）。
- [x] Stage2 提示词优化全落地（1.1–1.6，见 `stage2-optimization.md`）。
- [x] ASR 层热词（同音词类）跑通（蛇身 7:舌身 0）。

**2026-07-17（修丢音 + 三基建）**
- [x] **流式解码欠账修复**：sherpa `decode()` 单步 → `is_ready` drain 循环 + EOS `finalize_and_result`；删 warmup hack。真麦长句 partial 跟手、final 完整（复盘：`stage1-2-problems.md` 补章）。
- [x] **VAD 讲述式调优**：threshold 0.5 / min_silence 1.0s / min_speech 0.3s / max_speech 28s——自然停顿先于 sherpa 20s 急切强切（0.90/0.1s 词中腰斩）。
- [x] **Stage2 双通道段头合并 + 提示词规则**：批式权威 + 流式补头尾；删减尺度（标题/编号必留）、生僻英文原样保留、代办→待办等模式。
- [x] **热词双层种子 + 垃圾词过滤**：SEED_HOTWORDS（`aura.json` 可覆盖）进流式 recognizer + Stage2 store；`looks_like_concat` 拦 "APIdocker" 类拼接词。
- [x] **Stage2 移出消费线程**（原工程债）：`aura-stage2` worker；事件带 seq，Interim(N+1) 可先于 Final(N)，消费按 seq 归组；route 期间 partial 不冻结。
- [x] **R1 `GET /api/stream` SSE**（interim/final/status，带 seq）。
- [x] **R3 `GET /results` + `GET /context`** + `GET /api/recordings` + `GET /api/audio/:seq`。
- [x] **统一配置**：clap CLI（高频）> `aura.json`/`scout.json`（全量；CONF/DATA 命名空间，dev 应用目录 / prod `~/.desk-pilot/`）> 默认；业务 env 全删；`resolve()` 纯函数可测。
- [x] **tracing 日志体系**：门面归库、`shared::init_tracing` 归二进制（dev 人读 / release JSON、只写 stderr、`RUST_LOG` 过滤）。
- [x] **Storage 总管**（`aura-store::hub`）：`AudioArchive`（热层回放 + 日期命名 WAV 按期落盘 `recordings/<日期>/<时分秒>_<seq>.wav`，未落盘永不淘汰）+ `TurnLog`（`turns/<日期>.jsonl` 每 turn S1+S2 全量，与 wav 互链）+ recent 环；archive/wav 迁入 aura-store，store 独立无 aura 依赖。

---

## 🔴 近期：把 daemon 补完整

| # | 事项 | 价值 | 依赖/说明 |
|---|---|---|---|
| R2 | **`POST /control/{mic,speaker}` + 运行时配置** | 高 | scout 连接开关已有（`/api/control/scout`）；剩：音箱启停、运行时切模型/调热词 score。 |
| R4 | **`POST /annotate`（用户标注回传）** | 高 | geek-familiar 让用户对错词打标 → `(raw, corrected)` → Stage3 `CorrectionSample` 数据集。落点：`aura-store::hub` 加新组合件（同 TurnLog 模式）。 |
| R5 | **socket 传输定盘**：TCP localhost vs unix socket | 中 | 建议 unix socket（`/run/user/.../aura.sock`）+ TCP 兜底。 |
| R6 | **磁盘保留策略** | 中 | recordings/turns 按天/总量清理旧文件；当前只增不删。 |

## 🟡 中期：能力补全 + 接入 geek-familiar

| # | 事项 | 价值 | 说明 |
|---|---|---|---|
| M1 | **geek-familiar 接入** | 极高 | 连 socket、消费 `/api/stream`（**按 seq 归组**）、发 `/annotate`、接管 Stage3 调度（LLM tool-use 替换规则触发器）。 |
| M2 | **TTS 真模型** | 高 | `aura-tts` 接 Kokoro/Piper（sherpa-onnx）。NoopTts → 真后端，同 `Tts` trait。 |
| M3 | **Stage3 能力实现** | 高 | `FineTuner`（LoRA，吃 R4 的 CorrectionSample）、`ContextSummarizer`、`MemoryStore`。目前 stub。 |
| M4 | **aura-core legacy 迁移** | 中 | 删旧 `main.rs`/`ingest.rs`(energy VAD)/`pipeline.rs`/`routes.rs`；顺带定夺 aura-store 遗留 sqlite 4 表去留。 |
| M5 | **Stage3→Stage1 ASR 热词反馈** | 中 | 热词集变化时重建 recognizer（或 per-stream `create_stream_with_hotwords`）。当前闭环到 Stage2 + 静态种子层。真麦实证不在表的英文术语两路皆碎（Docker→DO CAR / GitHub→GUITAR）——**收益最直接的一条**。 |
| M6 | **Stage2 模型升级** | 中 | Qwen3.5-4B 提升整流质量 vs 保 1.7B + 强 few-shot；双模型显存预算（16GB GPU）。 |

## 🟢 长期：北极星（系统级 AI 秘书）

| # | 事项 | 价值 |
|---|---|---|
| L1 | **visual-rover agent team 协同** | 语音 intent=task → 派视觉/操作任务给 visual-rover（geek-familiar 调度）。audio-aura 提供"听准+意图"，visual-rover 提供"看见+操作"。 |
| L2 | **桌面精灵常驻秘书闭环** | 听 → 整流 → 意图 → 派活（写作/操作/查询）→ 汇报 → TTS 读出来。geek-familiar 是统一入口。 |
| L3 | **自适应学习闭环** | 用户标注 → 热词积累 → 检索增强校准(RAG) → LoRA 微调。把 Bevy 这类误读真正绑死（见 `adaptive-learning.md`）。 |
| L4 | **全双工可打断** | LiveKit 式 barge-in（用户开口→截断 TTS/LLM）。见 `livekit-port-notes.md`。 |

## ⚙️ 工程 / 基建债务

- [ ] `cargo build --workspace` 含 `native`(napi) 的 CI 验证；各 crate feature 矩阵测试。
- [x] ~~daemon 的 Pipeline 与 socket 同线程模型固化~~（2026-07-17：Stage2 worker + seq 事件契约落定，见 architecture「线程模型」）。
- [ ] Stage1Executor 的 `run()->!` 使单元测试只能用真模型；考虑加"有限事件"测试替身（或把 loop 抽成可中止）。
- [x] ~~录音 WAV 保存在抽象后丢失~~（2026-07-17：Utterance 带 pcm，AudioArchive 日期命名落盘 + 回放）。
- [ ] daemon `BASE` 常量仍指旧仓路径（`/workspaces/gui_agent/audio-aura/native`，靠旧目录残存才工作）；web_dist 默认解析应迁 shared loader。同类残留还在 aura-asr/aura-dcl/aura-core 的 examples/bins 里（约 10 处）。

## 决策待定（open）

- Stage3 调度：geek-familiar 用哪个 LLM 做 tool-use？（本地通用 vs 远程商用 vs 复用 1.7B——后者 tool-use 能力存疑）
- 模型：实时对话保 1.7B，写作/归纳用更大模型——双模型显存预算（16GB GPU）。
- 持久化：遗留 sqlite 4 表去留（M4 一并定）；turns jsonl 是否需要可查询索引（sqlite 化）。
