# Stage1 / Stage2：能力与流程（as-built 2026-08-17 · 边界范式）

> 代码为准。本文是语音识别两个 stage 的权威梳理。2026-08-17 从"就地修改范式"重构为
> **边界范式**（VadSegment/VadWindow，设计沿革见 `vad-segment-model.md`）；旧范式的
> `Utterance`/`Batch`/`MergeBatch` 契约已删除。
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
┌──────────────────── Stage1（音频 → 文本，边界范式）────────────────────┐
│ 段级流式会话（D1）：SOS 建 session → 喂帧 → ~0.5s 节流出 Interim       │
│   （携带真实 window_id + segment_id；只进 UI，不是 Stage2 输入）        │
│ VAD 间隔 (min_silence 1s) → EOS：                                     │
│   finalize 会话 → streaming_text                                      │
│   段 PCM 入 AudioStore（id） → 段级 batch（失败=None）                 │
│   → emit Batch { window_id, segments: 窗口全部段 }                    │
│ merge window (merge_gap 2.5s)：下一 SOS 间隔 ≥ 它 / 静默超时          │
│   → store.concat 拼接 PCM → 窗口级 batch 重跑（权威）                  │
│   → emit WindowEdge { window }（pcm: Arc）→ store.evict               │
└──────────────────────────────────────────────────────────────────────┘
   │  mpsc（Stage2 独立 worker 线程 aura-stage2；Interim 不走通道）
   ▼
┌──────────────────── Stage2（文本 → 纠偏文本，无状态）─────────────────┐
│ Batch      → calibrate_window：当前窗口全部段文本逐行联合整流（临时）   │
│ WindowEdge → calibrate_final：窗口 best_text（窗口 batch 优先）定稿     │
└──────────────────────────────────────────────────────────────────────┘
   ▼
daemon on_turn 回调 → SSE 数据面 /api/asr_stream → UI
   └ WindowFinal 同时落盘（recordings WAV + turns jsonl，按 window_id）+ Stage3 规则加词
```

## Stage1：音频 → 文本（ONNX 语音前端）

**位置**：`crates/aura-asr/src/executor.rs`（`OnnxStage1Executor`）+ 纯窗口决策核心
`WindowTracker`（可单测，无 I/O）+ `audio_store.rs`（PCM 按 id 存管）。ONNX 语音栈在
`dp-models::onnx`。

**两级实体**（`lib.rs` 数据契约区）：

| 实体 | 边界 | 内容 |
|---|---|---|
| `VadSegment` | VAD 间隔（min_silence） | id、audio_id、start_s/end_s、streaming_text（段级流式定稿）、batch_text: **Option**（远程失败合法） |
| `VadWindow` | merge window（merge_gap） | 段快照、拼接 streaming、窗口级 batch（**权威**）、pcm: Arc（settle 拼一次，store 随即 evict） |

### 流程细节

1. **采集**：ingest 线程写 AudioRing；断流 >2s 且当前段有 partial → 喂合成静音逼 EOS。
2. **段级流式**（D1）：SOS 时 `create_session`（廉价——模型共享，onnx.rs:159），EOS 时
   `finalize_and_result` 后丢弃。partial 每 ~15 窗（≈0.5s）解码、变化才发 `Interim`。
   **说话中无实时纠偏**（D2：勤快 Stage2 的 1s 路径已删除）。
3. **段定稿**（EOS）：双路文本都为空 → 噪声段丢弃；否则 PCM 入 store、段 batch
   （`.ok().filter(非空)` → Option）→ `Batch { window_id, segments }`（载荷即整个窗口，
   Stage2 无状态）。
4. **窗口定稿**：`WindowTracker` 判边界——下一 SOS 的间隔 ≥ merge_gap，或静默超时
   （`check_settle`，段进行中抑制）；`emit_window_edge` 拼 PCM → 窗口 batch 重跑 →
   `WindowEdge` → evict。merge_gap=0 → 每段独窗。
5. **AudioStore**：`Mutex<BTreeMap<id, PCM>>`，容量按样本（10min ≈19MB），超限逐最旧。

## Stage2：文本 → 纠偏文本（LLM 联合整流）

**位置**：`crates/aura-core/src/calibrator.rs`（`Stage2CalibratorImpl`）+ `prompt.rs`。

**无状态**：窗口状态完全由事件载荷携带（"移动左边界"= 下一个事件的载荷就是新窗口），
不存在内部缓冲失步。跑在独立 `aura-stage2` worker（composer.rs），LLM 耗时不卡 partial。

- `calibrate_window(window_id, segments)`：全部段 `best_text()` 逐行（`PromptBuilder::
  new_multi`，`<raw_transcript>` 信封内一行一段）联合整流 → `WindowCalibrated`（每 VAD
  间隔一次，替换同窗口上次结果）。
- `calibrate_final(window)`：优先窗口级 `batch_text`（整段重听），回退各段 best 拼接
  → `WindowFinal`（窗口粒度定稿，D3）。
- LLM 失败回退原文；用户纠正对（环形 20 条，POST /api/correct）优先级最高注入；
  热词 store 每次读最新（prompt 热词块仍处停用状态——小模型遵循不佳，见 prompt.rs）。

## 事件契约与线程

| 线程 | 职责 |
|---|---|
| `aura-stage1-ingest` | scout TCP → AudioRing |
| `aura-pipeline`（std 线程） | Stage1 consume loop（阻塞轮询，异步化仍是待办） |
| `aura-stage2` | LLM 联合整流 worker（mpsc 只收 Batch/WindowEdge） |
| `aura-socket`（tokio） | axum SSE：数据面 `/api/asr_stream` + 控制面 `/api/stream` |

| SSE 段类型（`AsrSegment`，aura-agent/view.rs） | 键 | 语义 |
|---|---|---|
| `interim` | window_id + segment_id | 段内流式 partial |
| `window_calibrated` | window_id | 窗口联合整流（临时，同窗替换） |
| `window_final` | window_id | 窗口定稿（raw=窗口batch 可空 / streaming 拼接 / calibrated） |
| `correction` | window_id | 用户纠正标记 |

识别事件走数据面（直推不节流）；设置变更走控制面（version ping → 重拉快照）。

存储按窗口：`record_final(FinalTurn{window_id,…})` → recordings WAV + turns jsonl，
`/api/audio/{window_id}`、`/api/recordings` 返回窗口 id。Stage3 规则触发器不变（吃定稿
calibrated 文本加词）。

## 迁移状态（2026-08-17）

- ✅ aura-asr / aura-core / aura-agent / aura-daemon 已切换新契约（定向构建+测试绿）。
- ⬜ **前端三处未迁移**（后续独立任务）：`apps/swift-ime`（bridge 的
  `CalibratedInterim`→`WindowCalibrated`、mock 测试 JSON 换 `window_calibrated`/
  `window_final` 标签）、`apps/geek-familiar`（app.rs 事件改挂）、
  `apps/audio-aura-devtools`（types.ts/App.tsx/UtteranceList 按 window_id 重键）。
  过渡期 `cargo build --workspace` 会在 swift-ime / geek-familiar 处失败，验证用 `-p` 定向。
