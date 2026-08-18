# Stage1 / Stage2：能力与流程（as-built 2026-08-17 · 边界范式）

> 代码为准。本文是语音识别两个 stage 的权威梳理。2026-08-17 从"就地修改范式"重构为
> **边界范式**（VadSegment/VadWindow，设计沿革见 `vad-segment-model.md`）；旧范式的
> `Utterance`/`Batch`/`MergeBatch` 契约已删除。
> 代码入口：Stage1 = `crates/aura-core/src/recognizer.rs`，Stage2 = `crates/aura-core/src/calibrator.rs`，
> 组装 = `crates/aura-core/src/pipeline.rs`，daemon = `apps/audio-aura/src/main.rs`。

## 总览：数据流

```
omni-scout /audio (TCP)
   │  ingest 线程 aura-stage1-ingest
   ▼
AudioRing（10min @16kHz mono）
   │  consume loop 每 tick 取 512 样本（32ms Silero 窗）
   ▼
┌──────────────────── Stage1（音频 → 文本，边界范式）────────────────────┐
│ 流式会话持续喂帧、段/窗口边界重置（D1 适配，见流程细节）                │
│   → ~0.5s 节流出 Interim（前瞻 id 键；只进 UI，不是 Stage2 输入）        │
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
┌──────────────────── Stage2（文本 → 纠偏文本，窗口状态机）──────────────┐
│ Batch      → calibrate_window：当前窗口全部段文本逐行联合整流，          │
│              结果覆盖窗口存档（每段一次）                                │
│ WindowEdge → finalize_window：**不跑 LLM**——取窗口最后一次联合整流存档   │
│              作为 VadWindow 纠偏字段，移动左边界（清空状态）             │
└──────────────────────────────────────────────────────────────────────┘
   ▼
daemon on_turn 回调 → SSE 数据面 /api/asr_stream → UI
   └ WindowFinal 同时落盘（recordings WAV + turns jsonl，按 window_id）+ Stage3 规则加词
```

## Stage1：音频 → 文本（ONNX 语音前端）

**位置**：`crates/aura-core/src/recognizer.rs`（`OnnxStage1Recognizer`，原 aura-asr executor.rs）+ 纯窗口决策核心
`WindowTracker`（可单测，无 I/O）+ `audio_store.rs`（PCM 按 id 存管）。ONNX 语音栈在
`dp-models::onnx`。

**两级实体**（`lib.rs` 数据契约区）：

| 实体 | 边界 | 内容 |
|---|---|---|
| `VadSegment` | VAD 间隔（min_silence） | id、audio_id、start_s/end_s、streaming_text（段级流式定稿）、batch_text: **Option**（远程失败合法） |
| `VadWindow` | merge window（merge_gap） | 段快照、拼接 streaming、窗口级 batch（**权威**）、pcm: Arc（settle 拼一次，store 随即 evict） |

### 流程细节

1. **采集**：ingest 线程写 AudioRing；断流 >2s 且当前段有 partial → 喂合成静音逼 EOS。
2. **流式会话 = 持续喂帧 + 段边界重置**（D1 的实际落地）：sherpa 的 VAD 只在段结束时
   吐出完整 segment——**SOS 是与 EOS 成对回溯发出的**（同批次、相差微秒），根本不存在
   "语音起点建会话"的时机（实测 fed=0 教训）。因此会话改为持续喂帧，在每次 EOS 终结
   （产出该段 streaming_text）和窗口 settle 后立即重置——每个会话恰好覆盖
   [上一边界, 本次 EOS] ≈ 单个段（含边界静音，静音不解码出字），段级归属保留。
   partial 每 ~15 窗（≈0.5s）解码、变化才发 `Interim`，键用 tracker 的前瞻
   (window_id, segment_id)（权威分组随 Batch 到达）。段的 `start_s` 由 PCM 时长回推。
   **停滞看门狗**：partial 非空但 ≥8s 无变化且无 EOS ⇒ VAD 从未锁定（音频低于
   threshold = 按设计应抛弃）⇒ 重置会话——微弱音频的残留（含流式幻觉复读）不得
   泄漏进下一个定稿段（2026-08-17 实测：35s 悬置会话把上一段幻觉文本卷进下一句）。
   真语音不受影响（停顿 ≥min_silence 即 EOS 定段重置，轮不到看门狗）。
   **说话中无实时纠偏**（D2：勤快 Stage2 的 1s 路径已删除）。
3. **段定稿**（EOS）：双路文本都为空 → 噪声段丢弃；否则 PCM 入 store、段 batch
   （`.ok().filter(非空)` → Option）→ `Batch { window_id, segments }`（载荷即整个窗口，
   Stage2 无状态）。
   ⚠️ 已知限制：段级/窗口级 batch 是消费循环内的**同步**调用——远程 ASR
   （mloader qwen3-asr）实测 ~3.5s/次，期间流式 partial 与 VAD 处理全部暂停。真实语速下
   EOS 间隔天然 >1s，无感；批量重放/长窗口时有可感知延迟。整改方向＝batch 移出消费
   线程（并入 roadmap R5 异步化）。
4. **窗口定稿**：`WindowTracker` 判边界——下一 SOS 的间隔 ≥ merge_gap，或静默超时
   （`check_settle`，段进行中抑制）；`emit_window_edge` 拼 PCM → 窗口 batch 重跑 →
   `WindowEdge` → evict。merge_gap=0 → 每段独窗。**单段窗口免重跑**：窗口 batch 的
   意义是跨段上下文重新整听——只有一段时拼接 PCM 与该段完全相同，直接复用段级
   batch 结果（含 None，不做失败重试）；单段是常态（merge 仅发生在 <merge_gap 的
   停顿后），故大多数窗口省掉一整次 batch 调用（远程 ~3.5s/次）。
5. **AudioStore**：`Mutex<BTreeMap<id, PCM>>`，容量按样本（10min ≈19MB），超限逐最旧。

## Stage2：文本 → 纠偏文本（LLM 联合整流）

**位置**：`crates/aura-core/src/calibrator.rs`（`Stage2CalibratorImpl`）+ `prompt.rs`。

**窗口状态机**：内部状态只有一个 `(当前窗口 id, 最后一次联合整流结果)` 对——每个
Batch 整体覆盖它；WindowEdge 取走存档作为该 VadWindow 的纠偏字段并清空（= 移动左
边界），**不再调用 LLM**（最后一个段的 Batch 到来时全窗口整流已完成）。事件在单一
worker 线程有序到达（Batch×N → WindowEdge），状态不可能失步。跑在独立 `aura-stage2`
worker（pipeline.rs），LLM 耗时不卡 partial。

- `calibrate_window(window_id, segments)`：全部段 `best_text()` 逐行（`PromptBuilder::
  new_multi`，`<raw_transcript>` 信封内一行一段）联合整流 → `WindowCalibrated`（每 VAD
  间隔一次，替换同窗口上次结果）并**覆盖窗口存档**。
- `finalize_window(window)`：返回存档（= 最后一次 `WindowCalibrated` 的文本）→
  `WindowFinal`（窗口粒度定稿，D3；route_ms ≈ 0）。防御路径：无存档时回退窗口
  best_text（理论不可达——窗口必有 Batch）。
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

- ✅ aura-core（含并入的原 aura-asr/aura-tts，2026-08-18）/ aura-agent / aura-daemon 已切换新契约（定向构建+测试绿）。
- ⬜ **前端三处未迁移**（后续独立任务）：`apps/swift-ime`（bridge 的
  `CalibratedInterim`→`WindowCalibrated`、mock 测试 JSON 换 `window_calibrated`/
  `window_final` 标签）、`apps/geek-familiar`（app.rs 事件改挂）、
  `apps/audio-aura-devtools`（types.ts/App.tsx/UtteranceList 按 window_id 重键）。
  过渡期 `cargo build --workspace` 会在 swift-ime / geek-familiar 处失败，验证用 `-p` 定向。
