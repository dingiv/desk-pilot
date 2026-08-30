# Stage1 / Stage2：能力与流程（as-built 2026-08-19 · 边界范式）

> 代码为准。本文是语音识别两个 stage 的权威梳理，含设计沿革（原 `vad-segment-model.md`
> 的 D1-D4 决策已并入附录）。旧范式的 `Utterance`/`Batch`/`MergeBatch`/`ContextWindow`
> 契约已删除。
> 代码入口：Stage1 = `crates/aura-core/src/recognizer.rs`，Stage2 = `crates/aura-core/src/calibrator.rs`，
> 组装 = `crates/aura-core/src/pipeline.rs`（`PipelineSpec` → `Pipeline::assemble` 全栈拼装 +
> 识别日志 + 窗口归档），daemon = `apps/audio-aura/src/main.rs`（config 解析 + socket）。

## 总览：数据流

```
omni-scout /audio (TCP)
   │  ingest 线程 aura-stage1-ingest（scout → ring；客户端可 ?chunk_ms=N 要求聚合推流）
   ▼
AudioRing（10min @16kHz mono）
   │  consume loop 取 512 样本（32ms 窗），Condvar 唤醒（无轮询、空闲零唤醒）
   ▼
┌──────────────────── Stage1（音频 → 文本，边界范式）────────────────────┐
│ 流式会话持续喂帧、段/窗口边界重置（D1 适配，见流程细节）                │
│   → ~0.5s 节流出 StreamFragment（前瞻 id 键；只进 UI，不是 Stage2 输入）│
│ 能量门：空闲(静音且冷却已过)时跳过 VAD/流式 NN（省 CPU，见流程细节 §6）│
│ VAD 间隔 (min_silence 1s) → EOS：                                     │
│   finalize 会话 → streaming_text                                      │
│   段 PCM 入 AudioStore（id） → 段级 batch（失败=None）                 │
│   → emit Batch { window_id, segments: 窗口全部段 }                    │
│ merge window (merge_gap 3.5s)：下一 SOS 间隔 ≥ 它 / 静默超时          │
│   → store.concat 拼接 PCM → 窗口级 batch 重跑（权威）                  │
│   → emit WindowEdge { window }（pcm: Arc）→ store.evict               │
└──────────────────────────────────────────────────────────────────────┘
   │  mpsc（Stage2 独立 worker 线程 aura-stage2；StreamFragment 不走通道）
   ▼
┌──────────────────── Stage2（文本 → 纠偏文本，窗口状态机）──────────────┐
│ Batch      → calibrate_window：当前窗口全部段文本逐行联合整流，          │
│              结果覆盖窗口存档（每段一次）                                │
│ WindowEdge → finalize_window：**不跑 LLM**——取窗口最后一次联合整流存档   │
│              作为 VadWindow 纠偏字段，移动左边界（清空状态）             │
└──────────────────────────────────────────────────────────────────────┘
   ▼
Pipeline run()（WindowEdge 臂）→ 识别日志 + record_final 落盘（recordings WAV +
   turns jsonl，按 window_id；storage 由 daemon 传入）
   ▼
daemon on_turn 回调 → SSE 数据面 /api/asr_stream → UI（WindowCalibration 时另触发 Stage3 规则加词）
```

## Stage1：音频 → 文本（ONNX 语音前端）

**位置**：`crates/aura-core/src/recognizer.rs`（`OnnxStage1Recognizer`）+ 纯窗口决策核心
`WindowTracker`（可单测，无 I/O）+ `audio_store.rs`（PCM 按 id 存管）。ONNX 语音栈在
`dp-models::onnx`（VAD Silero + 流式 Zipformer/x-asr + 批式 SenseVoice）。

**两级实体**（`lib.rs` 数据契约区）：

| 实体 | 边界 | 内容 |
|---|---|---|
| `VadSegment` | VAD 间隔（min_silence） | id、audio_id、start_s/end_s、streaming_text（段级流式定稿）、batch_text: **Option**（远程失败合法） |
| `VadWindow` | merge window（merge_gap） | 段快照、拼接 streaming、窗口级 batch（**权威**）、pcm: Arc（settle 拼一次，store 随即 evict） |

**流式引擎**：恒本地——`zipformer`（默认）或 `x-asr`（`asr.stream.model`；2026 百万小时
zh-en，自带标点；tokens.txt 必须保持官方"token id"两列格式）。

### 流程细节

1. **采集**：ingest 线程写 AudioRing（客户端可 `?chunk_ms=N` 请求 scout 聚合推流，
   `scout_chunk_ms` 配置）；断流 >2s 且当前段有 partial → 喂合成静音逼 EOS。
2. **流式会话 = 持续喂帧 + 段边界重置**（D1 的实际落地）：sherpa 的 VAD 只在段结束时
   吐出完整 segment——**SOS 是与 EOS 成对回溯发出的**（同批次、相差微秒），根本不存在
   "语音起点建会话"的时机（实测 fed=0 教训）。因此会话改为持续喂帧，在每次 EOS 终结
   （产出该段 streaming_text）和窗口 settle 后立即重置——每个会话恰好覆盖
   [上一边界, 本次 EOS] ≈ 单个段（含边界静音，静音不解码出字），段级归属保留。
   partial 每 ~15 窗（≈0.5s）解码、变化才发 `StreamFragment`，键用 tracker 的前瞻
   (window_id, segment_id)（权威分组随 Batch 到达）。段的 `start_s` 由 PCM 时长回推。
   **停滞看门狗**：partial 非空但 ≥8s 无变化且无 EOS ⇒ VAD 从未锁定（音频低于
   threshold = 按设计应抛弃）⇒ 重置会话——微弱音频的残留（含流式幻觉复读）不得
   泄漏进下一个定稿段。真语音不受影响（停顿 ≥min_silence 即 EOS 定段重置）。
   **说话中无实时纠偏**（D2：勤快 Stage2 的 1s 路径已删除）。
3. **段定稿**（EOS）：PCM 入 store（共享 `Arc<Vec<i16>>`）、**入队句级 batch job**（微秒级，
   消费循环不阻塞）→ `Batch { paragraph_id, sentences }`（载荷即整段，`batch_text: None`
   为 in-flight 态；句级 batch 结果异步经 `SentenceBatchReady` 回传）。噪声句不再 EOS
   丢弃（异步后 EOS 时刻只有流式文本，丢弃会吞"流式空 batch 有"的真实语音）——空句由
   段落折叠吸收，8s 停滞看门狗清幻觉。
   ✅ 曾为同步调用（远程 ~3.5s/次，阻塞流式/VAD → 吞句 bug）；2026-08-30 已移至
   `aura-batch` 单 worker 线程（roadmap R5 关闭，见 async-batch-design.md）。
4. **窗口定稿**：`WindowTracker` 判边界——下一 SOS 的间隔 ≥ merge_gap（3.5s），或静默超时
   （`check_settle`，段进行中抑制）；`emit_paragraph_edge` 拼 PCM（段落持 `Arc`）→
   **入段级重跑 job**（异步）→ `ParagraphEdge`（`batch_text: None` in-flight；结果经
   `ParagraphBatchReady` 回传）→ evict。merge_gap=0 → 每段独窗。**单段窗口免重跑**：
   只有一段时拼接 PCM 与该段完全相同，句级 batch job 已覆盖 → **不投递重跑 job**
   （复用句级结果）；单段是常态（merge 仅发生在 <merge_gap 的停顿后），故大多数
   窗口省掉一整次 batch 调用。定稿由 Stage2 worker 的**就绪门**触发：全句 batch 齐
   + 重跑齐（单段免重跑）→ LLM 整流一次。
5. **AudioStore**：`Mutex<BTreeMap<id, PCM>>`，容量按样本（10min ≈19MB），超限逐最旧。
6. **VAD 门控流式（2026-08-19）**：sherpa `VoiceActivityDetector::detected()` 提供**实时的**
   "正在检测到语音"信号——它是流式喂帧的唯一门卫：detected() 为 true 才喂流式（accept +
   解码），空闲零喂帧、零解码、零 CPU。起音翻转（detected false→true）时补喂最近 ~0.5s
   的 lead-in（Silero 过阈值有延迟，soft onset 靠它补进会话）。`accept_waveform` 与 `pcm`
   喂**完全相同**的帧 → 流式与 batch 听到同一段音频（共享 PCM 不变式）。替代了此前的
   能量门（RMS 代理）——VAD 是唯一语音门卫，语义一致（VAD 没检测到，流式也不出字）。

## Stage2：文本 → 纠偏文本（LLM 联合整流）

**位置**：`crates/aura-core/src/calibrator.rs`（`Stage2CalibratorImpl`）+ `prompt.rs`。

**无状态**（2026-08-30 batch 异步化后）：每次调用都是纯函数式的——输入是"整段全部句
的文本"（payload 即段落），**内部不存任何段落状态**。batch 异步后末句 batch 文本可能
晚于最后一个 `Batch` 到达，旧"存最后一次整流、定稿零 LLM 取存档"的不变式不再成立。
跑在独立 `aura-stage2` worker（pipeline.rs），LLM 耗时不卡 partial。pipeline 的
`Finalizer`（worker 线程内单线程独占、无锁）累积句级 batch 并做**就绪定稿**。

- `calibrate_paragraph(paragraph_id, sentences)`：全部句 `best_text()` 逐行（`PromptBuilder::
  new_multi`，`<primary_transcript>` 信封内一行一句）联合整流 → `SentenceCalibration`（每 VAD
  间隔一次，live 预览，替换同段上次结果）。
- `finalize_paragraph(paragraph)`：用全句最终 `best_text()`（句级 batch 已由 pipeline
  补齐；缺失句回退流式）**跑一次 LLM** → `ParagraphCalibration`（段粒度定稿，D3）。
  全空段零 LLM（直接回空）。定稿时机由 pipeline 就绪门控制：全句 batch 齐 + 段级重跑
  齐（单句段免重跑）——保证末句 batch 不会退化成流式。
- 纠偏输入源 `llm.input`（batch/stream/both）：batch 默认（`best_text()` 权威）；both 时
  双通道信封（`<primary_transcript>` + `<secondary_transcript>`）让 LLM 补回批式丢的句首。
- LLM 失败回退原文；用户纠正对（环形 20 条，POST /api/correct）优先级最高注入；
  热词 store 每次读最新（prompt 热词块仍处停用状态——小模型遵循不佳）。

## 事件契约与线程

| 线程 | 职责 |
|---|---|
| `aura-stage1-ingest`（pipeline spawn） | scout TCP → AudioRing（跑 Stage1 暴露的阻塞 `run_ingest`） |
| `aura-pipeline`（std 线程） | Stage1 consume loop（Condvar 事件驱动，无轮询，**零阻塞** —— batch 只入队 job） |
| `aura-batch`（std） | batch worker：串行跑阻塞 `AsrProvider::recognize`（句级/段级重跑），每 job 必出结果 → `SentenceBatchReady`/`ParagraphBatchReady`（跑 Stage1 暴露的阻塞 `run_batch_worker`） |
| `aura-stage2`（std） | LLM 联合整流 + 就绪定稿 worker（mpsc 收 Batch/ParagraphEdge/SentenceBatchReady/ParagraphBatchReady；定稿 = 全句 batch 齐 + 重跑齐 → LLM 一次） |
| `aura-socket`（tokio） | axum SSE：数据面 `/api/asr_stream` + 控制面 `/api/stream` |

> **线程归属约束**（2026-08-30）：Stage1/Stage2 模块**不 spawn 任何线程**，只暴露阻塞
> 函数；上表四个线程全部由 `pipeline.rs` 创建。

| SSE 段类型（`AsrSegment`，aura-agent/view.rs） | 键 | 语义 |
|---|---|---|
| `stream_fragment` | window_id + segment_id | 流式模型每次产出的当前段文本（live partial + EOS 定稿） |
| `batch_segment` | window_id + segment_id | batch 识别完单个段（EOS）——该段 batch 文本 |
| `batch_window` | window_id | batch 识别完整个窗口（settle）——整窗重跑权威 raw_text |
| `segment_calibration` | window_id | 每次 Batch 联合纠偏完成——整窗已校准文本（同窗替换） |
| `window_calibration` | window_id | 窗口纠偏定稿（window 关闭）——最后一次联合整流结果 |
| `correction` | window_id | 用户纠正标记 |

识别事件走数据面（直推不节流）；设置变更走控制面（version ping → 重拉快照）。

存储按窗口：`record_final(FinalTurn{window_id,…})` → recordings WAV + turns jsonl，
`/api/audio/{window_id}`、`/api/recordings` 返回窗口 id。Stage3 规则触发器不变（吃定稿
calibrated 文本加词）。

## 迁移状态（2026-08-19）

- ✅ aura-core / aura-agent / aura-daemon 已切换 5 事件协议 + 边界范式（定向构建+测试绿）。
- ✅ `apps/swift-ime`（bridge 挂 `StreamFragment`/`SegmentCalibration`/`WindowCalibration`）、
  `apps/geek-familiar`（app.rs 事件改挂 + ConversationTurn 按 window_id 重键）。
- ⬜ **`apps/audio-aura-devtools` 未迁移**（后续独立任务）：types.ts/App.tsx/UtteranceList
  换 5 事件协议并按 window_id 重键。

## 附录：设计沿革与决策记录（原 vad-segment-model.md）

### 动机（为什么推翻"就地修改"范式）

1. **内存**：录音 PCM 今天在 `Utterance`/`MergeAccum`/`Storage` 间整块克隆；改由专门
   store 持有，实体只存 id。
2. **计算**：batch 识别从"每个 EOS 对**累积**音频重跑"（n 段 utterance 总量 O(n²) 音频）
   变为"每段一次 + 窗口定稿拼接重跑一次"（O(n)）。
3. **语义**：`VadSegment` / `VadWindow` 扶正为一等实体，替代隐式的 `MergeAccum`；
   事件从"同 seq 就地更新（修改范式）"变为 **append-only 片段 + 边界标记**。
4. **Stage2**：从"单 utterance 整流"升级为"**窗口内多句联合整流**"——跨句上下文
   改善同音字、标点、连贯性。

### 已决决策（2026-08-17 拍板，均已实现）

- **D1 流式会话粒度 → 段级会话**：每个 VadSegment 独立开流式会话，EOS 定稿。
  接受段边界编码器上下文丢失（段首字可能略差）；每段有完整流式结果，拼接即窗口流式。
  **实施修正（实测）**：sherpa VAD 的 SOS 是与 EOS 成对**回溯**发出的，"SOS 建会话"
  没有时机——落地为**持续喂帧 + 每段 EOS / 窗口 settle 后重置会话**，效果等价于段级会话。
- **D2 说话中实时纠偏 → 砍掉**：Batch 只在 VAD 间隔触发；说话中 UI 只显示 raw
  partial，纠偏文本在首个 VAD 间隔后出现（~1s 滞后）。省掉每秒一次的 LLM 调用。
- **D3 WindowEdge 产出 → 窗口级 Final**：一个窗口定稿一条（窗口内多句联合整流的
  完整文本）。UI 时间线：段实时生长 → 窗口关闭收拢为一条定稿。
- **D4 迁移策略 → 直接替换**：一次性 breaking——executor / composer / calibrator /
  daemon / aura-agent SDK 已同步切换；前端 swift-ime / geek-familiar 已迁移，devtools 暂缓。

### 实现补充裁决

- **Stage2 窗口状态机**：Batch 事件每次携带当前窗口全部段（载荷即窗口）；内部仅存
  `(当前窗口 id, 最后一次整流结果)`，WindowEdge 消费并清空（定稿不跑 LLM）。
- **死代码清除**：`Decision`/`parse_decision`/`ContextWindow`/`AudioChunk` 删除，
  Stage2 校准直接返回 `String`。
- **窗口 PCM**：settle 拼接一次为 `Arc<Vec<i16>>` 挂在 VadWindow 上（窗口 batch 与
  daemon 落盘共用），store 随即 evict。
