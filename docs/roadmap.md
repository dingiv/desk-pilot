# 路线图（未完成事宜）

> 2026-07-15 整理。对照 `architecture.md` 现状，按 horizon 排列，每条标「状态 / 价值 / 依赖」。北极星：[[ai-secretary-north-star]]。

## ✅ 已完成（脊柱）

- [x] 6-crate 拓扑（asr/dcl/tts/core/agent/daemon），依赖无环。
- [x] 三抽象：`Stage1Executor`（吞掉 stage12_live 面条）、`Stage2Calibrator`、`Stage3Agent` 能力层。
- [x] `Pipeline` 组装 + 薄壳 `stage12_live` example；行为对齐通过。
- [x] daemon 二进制 + socket 骨架（`/health`、`/context` 真实，余 stub）+ Stage3 规则触发器闭环（store 空→加 Rust/Bevy→`/context` 显示）。
- [x] Stage2 提示词优化全落地（1.1–1.6，见 `stage2-optimization.md`）。
- [x] ASR 层热词（同音词类）跑通（蛇身 7:舌身 0）。

---

## 🔴 近期：把脊柱补成可用 daemon

| # | 事项 | 价值 | 依赖/说明 |
|---|---|---|---|
| R1 | **`GET /stream/asr` SSE 做实** | 极高 | geek-familiar 悬浮窗"实时显示识别+纠偏"最依赖的接口。推 `Interim{partial}` + `Final{utterance,decision}`。需把 Pipeline 的 `on_turn` 事件桥到 tokio broadcast channel → SSE。 |
| R2 | **`POST /control/{mic,speaker}` + 运行时配置** | 高 | 启停录音/音箱、切模型、调热词 score 等。Stage1Executor 暴露 `set_active` + 一个运行时 config 句柄。 |
| R3 | **`GET /results` + `GET /context`** | 高 | 查 S1/S2 历史 + ContextWindow 当前内容。需 daemon 维护一份近期 turns（内存即可，持久化走 store）。 |
| R4 | **`POST /annotate`（用户标注回传）** | 高 | geek-familiar 让用户对错词打标 → 回传 `(raw, corrected)` → 喂 Stage3 的 `CorrectionSample` 数据集（渐进式提升的原料）。接 `aura-store` 落库。 |
| R5 | **socket 传输定盘**：TCP localhost vs unix socket | 中 | 本地 geek-familiar↔daemon。建议 unix socket（`/run/user/.../aura.sock`）+ TCP 兜底。 |

## 🟡 中期：能力补全 + 接入 geek-familiar

| # | 事项 | 价值 | 说明 |
|---|---|---|---|
| M1 | **geek-familiar 接入** | 极高 | geek-familiar 作为秘书 agent：连 socket、消费 `/stream/asr`、发 `/annotate`、**接管 Stage3 调度**（用 LLM tool-use 决定何时加词/微调/归纳，替换 daemon 的规则触发器）。 |
| M2 | **TTS 真模型** | 高 | `aura-tts` 接 Kokoro/Piper（sherpa-onnx）。文本来自 agent 层（reply / 读出校准句 / 汇报）。NoopTts → 真后端，同 `Tts` trait。 |
| M3 | **Stage3 能力实现** | 高 | `FineTuner`（LoRA，从 CorrectionSample 数据集）、`ContextSummarizer`（长对话归纳）、`MemoryStore`（跨会话键值 + 持久化）。目前只 stub。 |
| M4 | **aura-core legacy 迁移** | 中 | 删旧 `main.rs`/`ingest.rs`(energy VAD)/`pipeline.rs`(handle_turn)/`routes.rs`——daemon 已取代。迁移前确认 geek-familiar/TS 不再依赖旧 :9090 API。 |
| M5 | **Stage3→Stage1 ASR 热词反馈** | 中 | sherpa 在 `OnlineRecognizer` 创建时烘焙热词。要动态化：热词集变化时重建 recognizer，或用 per-stream `create_stream_with_hotwords`。当前闭环只到 Stage2（LLM 层热词）。 |
| M6 | **Stage2 模型升级** | 中 | Qwen3.5-4B（doc 6.2 甜蜜点）提升整流质量，代价延迟。或保 1.7B + 更强 few-shot。 |

## 🟢 长期：北极星（系统级 AI 秘书）

| # | 事项 | 价值 |
|---|---|---|
| L1 | **visual-rover agent team 协同** | 语音 intent=task → 派视觉/操作任务给 visual-rover（geek-familiar 调度）。audio-aura 提供"听准+意图"，visual-rover 提供"看见+操作"。 |
| L2 | **桌面宠物常驻秘书闭环** | 听 → 整流 → 意图 → 派活（写作/操作/查询）→ 汇报 → TTS 读出来。geek-familiar 是统一入口。 |
| L3 | **自适应学习闭环** | 用户标注 → 热词积累 → 检索增强校准(RAG) → LoRA 微调。把 Bevy 这类误读真正绑死（见 `adaptive-learning.md`）。 |
| L4 | **全双工可打断** | LiveKit 式 barge-in（用户开口→截断 TTS/LLM）。见 `livekit-port-notes.md`。 |

## ⚙️ 工程 / 基建债务

- [ ] `cargo build --workspace` 含 `native`(napi) 的 CI 验证；各 crate feature 矩阵测试。
- [ ] daemon 的 Pipeline 与 socket 同线程模型固化（当前 main 线程跑 Pipeline，tokio 跑 socket；SSE 上线后事件桥要稳妥）。
- [ ] Stage1Executor 的 `run()->!` 使单元测试只能用真模型；考虑加一个 trait 的"有限事件"测试替身（或把 loop 抽成可中止）。
- [ ] 录音 WAV 保存在抽象后丢失（Utterance 不带 PCM）——若 bench 需要回放，给 Utterance 加可选 PCM 或并行事件。

## 决策待定（open）

- Stage3 调度：geek-familiar 用哪个 LLM 做 tool-use？（本地通用 vs 远程商用 vs 复用 1.7B——后者 tool-use 能力存疑）
- 模型：实时对话保 1.7B，写作/归纳用更大模型——双模型显存预算（16GB GPU）。
- 持久化：`aura-store`(rusqlite) 复用现有 4 表，还是为 Stage3 记忆/标注加新表。
