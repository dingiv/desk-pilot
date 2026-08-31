# Stage1 / Stage2:能力与流程(as-built 2026-08-31 · 边界范式)

> 代码为准。本文是语音识别两个 stage 的权威梳理,含数据契约与设计沿革(D1-D4 附录)。
> 代码入口(round23 文件夹化):Stage1 = `crates/aura-core/src/pipeline/`
> (consume=消费循环 / recognizer=资源 / tracker=边界数学 / stream=流式任务 / vad=采音+VAD),
> Stage2 = `pipeline/calibrator.rs` + `tasks.rs`(任务壳),
> 组装 = `pipeline/mod.rs`(`PipelineSpec → Pipeline::assemble`),daemon = `apps/audio-aura/src/main.rs`。
> 架构全貌见 [pipeline.md](pipeline.md);排障见 [debugging.md](debugging.md)。

## 总览:数据流

```
omni-scout /audio (TCP)
   │  ① ingest(blocking 池常驻线程;pipeline/vad.rs;自动重连)
   ▼
AudioRing(10min @16kHz mono)+ Notify 唤醒
   │  ③ 消费循环(异步任务,pipeline/consume.rs;取 512 样本=32ms 帧)
   ▼
┌──────────────────── Stage1(音频 → 文本,边界范式)────────────────────┐
│ VAD(pipeline/vad.rs VadFront)门控:起音即开段(时间戳真 id)+补喂 lead-in │
│ 流式任务(独立 tokio::task,pipeline/stream.rs;一对通道):              │
│   → ~0.3s 节流出 StreamFragment(只进 UI,不是 Stage2 输入)             │
│ VAD 间隔(min_silence 1s)→ EOS:定稿交接(回执同通道)→ 句 PCM           │
│   → Batch { paragraph_id, sentences: 段内全部句 }                       │
│ merge_gap(部署 3.5s)→ 段关闭:settle / 下一 SOS 间隔 ≥ 它              │
│   → store.concat 拼接 PCM → ParagraphEdge(整段重跑由段任务自建)        │
└──────────────────────────────────────────────────────────────────────┘
   │  tokio 通道 → select! 主循环(唯一发射点,统一留痕)
   ▼
┌──────────────────── 编排(pipeline/mod.rs + tasks.rs)─────────────────┐
│ Batch → 句任务(spawn_blocking recognize_once)→ BatchSentence          │
│ BS 到达 → live 整流任务(段内链式串行,输入=双通道)→ SentenceCalibration │
│ ParagraphEdge → 段任务(join 句任务 + live 链尾 → 段重跑(多句)→        │
│   BatchParagraph → 定稿整流一次 → 归档)→ ParagraphCalibration          │
└──────────────────────────────────────────────────────────────────────┘
   ▼
daemon on_turn 回调 → SSE 数据面 /api/asr_stream → 前端级联折叠(见 pipeline.md §8)
   + record_final 落盘(recordings WAV + turns jsonl,按 paragraph_id)
```

## Stage1:音频 → 文本(ONNX 语音前端)

**位置**:`pipeline/`(`vad.rs` 采音+VAD 检测 / `recognizer.rs` 资源 / `consume.rs` 消费循环
/ `stream.rs` 流式任务 / `tracker.rs` ParagraphTracker 纯边界数学,可单测无 I/O)+
`audio_store.rs`(PCM 按 id 存管)。ONNX 语音栈在 `dp-models::onnx`(VAD Silero +
流式 Zipformer/x-asr + 批式 SenseVoice)。

**两级实体**(`lib.rs` 数据契约区):

| 实体 | 边界 | 内容 |
|---|---|---|
| `VadSentence` | VAD 间隔(min_silence) | id、audio_id、start_s/end_s、streaming_text(句级流式定稿)、batch_text: **Option**(远程失败合法) |
| `VadParagraph` | merge 段(merge_gap) | 句快照、拼接 streaming、段级 batch(重跑,权威)、pcm: Arc(settle 拼一次,store 随即 evict) |

**流式引擎**:恒本地——`zipformer`(默认)或 `x-asr`(`asr.stream.model`;自带标点;
tokens.txt 必须保持官方"token id"两列格式)。

### 流程细节

1. **采集**:ingest 写 AudioRing(客户端可 `?chunk_ms=N` 请求聚合推流);断流 >2s 且当前
   句有 partial → 喂合成静音逼 EOS。
2. **流式任务 = 独立 tokio::task(round21)**:消费循环只转发帧指令(`Onset`/`Feed`/
   `Reset`/`Finalize`),accept/decode(ONNX 前向)全在流式任务里——与 VAD/分句/段落定稿
   零共享执行流;partial 回传后仍由消费循环发射(两任务汇于同一事件出口,全序不破)。
   会话是**持续喂帧 + 边界重置**(D1 落地:sherpa 的 SOS 与 EOS 成对回溯,不存在"起点
   建会话"时机),恰好覆盖 [上一边界, 本次 EOS] ≈ 单句。partial 每 9 窗(≈0.3s)解码、
   变化才发 `StreamFragment`;EOS 定稿回执走同一通道(`Finalized`,round24)。
   **停滞看门狗**:partial 非空但 ≥8s 无变化且无 EOS ⇒ VAD 从未锁定 ⇒ 重置会话
   (微弱音频残留/流式幻觉不得卷入下一句)。
   **说话中无实时纠偏**(D2:1s 路径已删)。
3. **句定稿**(EOS):流式任务 finalize 交接(PCM + 定稿文本,几十 ms)→ PCM 入 store
   (共享 `Arc`)→ `Batch { paragraph_id, sentences }`(载荷即整段,`batch_text: None`
   为 in-flight;**句级 batch 由 pipeline 句任务自建**——主循环收到 Batch 即
   `spawn_blocking(recognize_once)`,结果以 `BatchSentence` 回传)。噪声句不在 EOS 丢弃
   (异步后 EOS 时刻只有流式文本,丢弃会吞"流式空 batch 有"的真实语音)。
4. **段定稿**:`ParagraphTracker` 判边界——下一 SOS 间隔 ≥ merge_gap,或静默超时
   (`check_settle`,句进行中/speaking 抑制);起音即开段(rising edge 分配时间戳 id,
   partial 从第一条起携带真键);空段静默满 merge_gap 即 GC。`emit_paragraph_edge` 拼
   PCM(段落持 `Arc`)→ `ParagraphEdge` → evict。**单句段免重跑**:只有一句时拼接 PCM
   与该句完全相同,复用句级结果。段任务 join 全部句任务(就绪门)+ live 链尾 → 段重跑
   (多句;`spawn_blocking` + 15s 兜底,**PCal 必发**)→ 定稿整流一次。
5. **AudioStore**:`Mutex<BTreeMap<id, PCM>>`,容量按样本(10min ≈19MB),超限逐最旧。
6. **VAD 门控流式**:`detected()` 实时信号是流式喂帧的唯一门卫(空闲零喂帧零解码);
   起音翻转时补喂最近 ~0.5s lead-in(soft onset 靠它进会话);`accept_waveform` 与
   `pcm` 喂完全相同的帧 → 流式与 batch 听到同一句音频(共享 PCM 不变式)。

## Stage2:文本 → 纠偏文本(LLM 联合整流)

**位置**:`pipeline/calibrator.rs`(`Stage2CalibratorImpl`)+ `prompt.rs`;任务壳在
`pipeline/tasks.rs`(`spawn_blocking`,LLM 耗时不卡 partial)。

**无状态**:每次调用都是纯函数式——输入是"整段全部句的文本"(payload 即段落)。

- `calibrate_paragraph(paragraph_id, sentences)`:全部句 `best_text()` 逐行联合整流 →
  `SentenceCalibration`。**触发点 = 每句 BS 到达**(round17b:架构要求"batch 完成 →
  之后纠偏,先后明确"),段内链式串行(SC 顺序 = 段落生长序);输入带 `segment_id` =
  覆盖上界(round20b,前端零派生状态即知覆盖谁)。
- `finalize_paragraph(paragraph)`:用全句最终 `best_text()`(句任务 join 回填;缺失句
  回退流式)**跑一次 LLM** → `ParagraphCalibration`(段粒度定稿,D3)。全空段零 LLM。
- 纠偏输入源 `llm.input`:**both 为默认**(双通道信封,LLM 对照补回批式丢的句首);
  batch/stream 为显式降级。
- LLM 失败回退原文;用户纠正对(环形 20 条,POST /api/correct)优先级最高注入;
  热词 store 每次读最新(prompt 热词块停用——小模型遵循不佳)。

## 事件契约(wire)与执行载体

**SSE 事件类型**(`AsrEvent`,aura-agent/view.rs;**字段名冻结旧词汇**——Rust 侧
sentence/paragraph 改名经 serde rename 回 `window_id`/`segment_id`,预构建 Web SPA 与
存量日志不受影响):

| SSE type | 键 | 语义 |
|---|---|---|
| `stream_fragment` | window_id + segment_id | 流式 partial(live 生长)+ EOS 定稿一条 |
| `batch_sentence` | window_id + segment_id | 句级 batch 结果(该句 1s 空白后) |
| `batch_paragraph` | window_id | 整段重跑(多句段;单句段不发) |
| `sentence_calibration` | window_id + **segment_id(覆盖上界)** | 联合纠偏(BS 到达触发,同段 REPLACed) |
| `paragraph_calibration` | window_id | 段定稿(该段最后一条事件,必发) |
| `correction` | window_id | 用户纠正标记 |

**执行载体**(round12+ 任务结构,round21 流式独立;Stage1/Stage2 模块不 spawn 线程,
全部由 `pipeline/` 创建):

| 载体 | 职责 |
|---|---|
| blocking 池线程 ×1(常驻) | ingest:scout TCP → ring(`vad::ingest_loop`) |
| 异步任务 | 消费循环(`consume::run`:VAD/分句/段落决策) |
| 异步任务 | 流式识别(`stream::run_stream_worker`,一对通道) |
| 异步任务 + spawn_blocking | 句任务(batch recognize)/ live 整流 / 段任务(重跑+定稿+归档) |
| 异步 future | `select!` 主循环(唯一发射点,统一留痕) |
| 主线程 tokio | axum SSE:数据面 `/api/asr_stream` + 控制面 `/api/stream` |

识别事件走数据面(直推不节流);设置变更走控制面(version ping → 重拉快照)。
存储按段:`record_final` → recordings WAV + turns jsonl,`/api/audio/{id}`、
`/api/recordings`。Stage3 规则触发器不变(吃定稿 calibrated 文本加词)。

## 附录:设计沿革与决策记录(原 vad-segment-model.md,2026-08-17 拍板)

> 历史记录:文中 VadSegment/VadWindow 为当时的实体名,现已更名 VadSentence/VadParagraph
> (round13+,wire 字段仍冻结旧词汇,见上表)。

- **D1 流式会话粒度 → 句级会话**:实施修正——sherpa VAD 的 SOS 与 EOS 成对回溯,
  落地为持续喂帧 + 每句 EOS / 段 settle 后重置会话,效果等价句级会话。
- **D2 说话中实时纠偏 → 砍掉**:说话中 UI 只显 raw partial,纠偏在首个 VAD 间隔后
  (batch 完成 → 之后纠偏)。
- **D3 段关闭产出 → 段级 Final**:一段定稿一条(段内多句联合整流的完整文本)。
- **D4 迁移策略 → 直接替换**:executor/calibrator/daemon/aura-agent/前端同步切换;
  `audio-aura-devtools` 未迁移(旧 SPA,忽略新字段)。
- **Stage2 窗口状态机 → 无状态化**(batch 异步后):载荷即段落,内部不存状态;
  就绪定稿由段任务 join 结构承担。
- **段 PCM**:settle 拼接一次为 `Arc<Vec<i16>>` 挂在 VadParagraph 上(段重跑与落盘
  共用),store 随即 evict。
