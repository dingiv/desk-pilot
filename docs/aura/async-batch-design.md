# Stage1 batch 异步化:把 batch 识别移出消费线程(设计)

> **状态:已实现(2026-08-30,dev 分支)。** 代码为准。
> **落地形态与本文设计有两处差异**(实现时的用户约束):
> 1. **单 batch worker(N=1 FIFO)** 而非 §5.1 的 N=2 池 —— 用户指定"添加一个独立的线程"。
>    后果:连续说话时 job 可能积压(吞吐 ≈ 1 句/3.5s),定稿延迟受排队影响(§6 B1 的
>    取舍);要升级成池只需在 `pipeline.rs` 对同一条 job 通道多 spawn 几个线程,消费循环
>    与事件契约不变。
> 2. **线程创建全部收归 `pipeline.rs`** —— Stage1/Stage2 只暴露阻塞函数
>    (`OnnxStage1Recognizer::run_ingest` / `run_batch_worker` / `Stage1Recognizer::run`),
>    stage 模块内部 **不 spawn 任何线程**;ingest 线程的创建也一并收归(原设计里 ingest
>    仍由 recognizer 自建)。
> 未落地(属 P4):job 超时 + tick 线程就绪兜底。
>
> 本文是 [`pipeline-optimization.md`](pipeline-optimization.md) 的 **P0(地基)**;整条管线的
> 分层优化(P1 整流去冗余 / P2 短段免重跑 / P3 首字延迟 / P4 鲁棒性)见该文档。
> 关联 bug:说话间隔 1–3.5s 时,前端预览里**第一句概率性被吞**(段落被过早切分)。
> 代码入口:Stage1 = `crates/aura-core/src/recognizer.rs`(消费循环 + 边界决策),
> Stage2 = `crates/aura-core/src/calibrator.rs`(联合整流),组装 = `crates/aura-core/src/pipeline.rs`
> (Stage2 worker + 事件路由 + 归档),前端折叠(消费方)= `crates/ime-core/src/voice_state.rs`。
> 契约权威:`docs/aura/stages.md`。

---

## 1. 问题

**复现**:说完第一句,停顿 1–3.5s,再说第二句。概率性出现:

```
我说完第一句话.  [2s]  我说了第二句话.

bug  预览:我说完第一句话  →  我说了第二句话          ← 第一句被吞
期望 预览:我说完第一句话  →  我说完第一句话. 我说了第二句
```

**现象**:第一句掉成一个独立定稿(另一条候选),第二句进了新段落,两段没有拼在一起。

---

## 2. 根因

**一句话**:句级 batch 是**消费主线程上的同步阻塞调用**(远程 ~3.5s/次),阻塞期间墙钟照走,
恢复瞬间 `check_settle` 用墙钟判定"已静默 ≥ merge_gap",把段落**过早定稿**——此时第二句的
音频还压在 AudioRing 里没被处理,自然落进新段落。

**阻塞链**(全部在 `aura-pipeline` 这一个 std 线程上):

1. 句 1 EOS → `finalize_sentence`(`recognizer.rs:804`):
   `self.batch_asr.recognize(&sentence_pcm, sr)` **同步**执行,远程 ~3.5s。
   这 3.5s 里消费循环**不取帧、不跑 `check_settle`、不喂流式**——第二句音频全堆在 ring 里。
2. 阻塞期间 `speaking` 恒 false:句 1 EOS 时流式会话已 reset(空),阻塞期间又没喂第二句
   音频 → `last_partial` 空 → `speaking = false`(`recognizer.rs:933`)。
   于是 `check_settle` 的 `speaking` 抑制**没有兜住**。
3. batch 返回,循环恢复,**第一步就是 `check_settle`**(`recognizer.rs:954`):
   `now - last.end_s`(墙钟)≥ `merge_gap` → 判"静默已满" → `emit_paragraph_edge`
   把段落 1 定稿(只含句 1)。
4. 第二句随后才被处理,`tracker` 已 `take_open()` → 句 2 拿**新段落 id**
   (`prospective`,`recognizer.rs:582`)。前端按 `current_paragraph` 渲染 → 组合预览只剩句 2,
   句 1 掉成独立定稿(`voice_state.rs:335` 的 `ParagraphCalibration` 臂)。

**为什么概率性**:触发条件 = `batch 延迟 ≥ 到 merge_gap 截止点的剩余时间`。
batch ≈ 3.5s、`merge_gap` = 3.5s,几乎每次句末都贴线,掷硬币:

- batch 早于截止点返回(本地 batch 快 / 网络快)→ 第二句音频在段落还开着时被处理 → 正常拼接。
- batch 晚于截止点返回(远程 ~3.5s)→ 恢复瞬间 `check_settle` 命中 → 误切。

**为什么只在 1–3.5s 区间**:`≥3.5s` 本就该切段(行为正确);`<1.0s` 是同句不切。
卡在这个"同段"区间 + batch 慢,才触发。

> `recognizer.rs:898` 的 TODO 已记录此问题:"batch 调用还在消费线程内同步执行
> (远程 ~3.5s/次会暂停流式)"。本文即该 TODO 的完整设计。

---

## 3. 现状架构(as-is)

**线程**:

| 线程 | 职责 | 是否阻塞音频 |
|---|---|---|
| `aura-stage1-ingest` | scout TCP → AudioRing | 否(独立) |
| `aura-pipeline`(std) | **Stage1 消费循环**:取帧 / VAD / 喂流式 / **同步 batch** / 边界决策 | **是(batch 在这)** |
| `aura-stage2`(std) | LLM 联合整流(mpsc 只收 `Batch` / `ParagraphEdge`) | 否(独立) |
| `aura-socket`(tokio) | axum SSE(数据面 `/api/asr_stream` + 控制面 `/api/stream`) | 否 |

**事件流**(消费循环 → Stage2 worker → 前端):

```
说话 → VAD 门控流式(StreamFragment,inline 直发前端)
  ↓ 静音 ≥ min_silence (EOS)
finalize_sentence:
  streaming_text = 流式 finalize(快,本地)
  batch_text     = batch_asr.recognize()   ← ★ 同步阻塞 ~3.5s
  tracker.on_eos → 段落决策
  [若 gap ≥ merge_gap 先定稿上段] emit_paragraph_edge:
     整段 batch 重跑 = batch_asr.recognize(concat PCM)  ← ★ 同步阻塞
     → ParagraphEdge
  emit StreamFragment(定稿流式) + Batch(带全部句,含 batch_text)
  ↓ 静默 ≥ merge_gap / flush / 大 gap
ParagraphEdge → Stage2 finalize(零 LLM,取存档)→ ParagraphCalibration
```

**关键点**:`aura-stage2` 的 LLM 调用**不**卡音频(独立线程);**唯一**卡消费线程的是
两处 batch(`recognizer.rs:804` 句级 + `recognizer.rs:620` 段级重跑)。把这两处移出消费线程,
bug 即根除。

---

## 4. 设计目标 / 非目标

**目标**

1. **消费循环永不被 batch 阻塞** —— 流式 / VAD / `check_settle` 持续运行,`speaking` 保持
   真实值,段落不再被墙钟误切。
2. **batch 吞吐不降** —— 并行识别,定稿延迟不劣于现状(现状句级 batch 本就是在说话间隙
   "免费"跑的)。
3. **前端契约零改动** —— `AsrEvent` wire 协议 **FROZEN**(`aura-agent/view.rs`),
   改动**完全收敛在 `aura-core` 内**(Stage1 + pipeline + Stage2),daemon / SSE / 前端不动。
4. **顺序与定稿正确性** —— **定稿 `ParagraphCalibration` 必在该段落全部 `BatchSentence` 之后**
   (且用最佳文本);`BatchParagraph` 允许与部分 `BatchSentence` 交错(池并行),前端按 id 折叠
   不受影响(§5.7)。定稿文本用**最佳可得**文本(batch 优先,流式回退)。

**非目标(本轮不做,见 §10)**

- 段级 batch 重跑的跳过/降级(它是"整段跨句重听"的权威文本,保留现状语义)。
- batch 失败重试策略(沿用 `Option` 回退流式)。
- 多段落并发定稿(边界范式下同一时刻只有一个开放段落,天然串行)。

---

## 5. 设计

### 5.1 核心思想

把两处同步 batch 调用替换为**投递到独立 batch 工作池的异步 job**。消费循环只在 EOS /
settle 时**入队** job(非阻塞,微秒级)并继续;batch 池完成后把结果作为**新事件**发回
pipeline,由 pipeline 累积并按"就绪"条件触发定稿。

```
消费循环(不再阻塞)              batch 工作池(N 线程)            Stage2 worker(pipeline)
  EOS → 入队句级 job ──────────►  recognize() ──SentenceBatchReady──►  累积句 batch
  settle → 入队段级重跑 job ───►  recognize() ──ParagraphBatchReady──►  就绪 → 定稿
  继续取帧/VAD/流式/check_settle                              (LLM 联合整流)
```

### 5.2 新组件:batch 工作池(`BatchPool`)

- **位置**:新增 `crates/aura-core/src/batch_pool.rs`(或并入 `recognizer.rs`)。
- **形态**:N 个 std 线程(默认 **N=2**,可配),共享一个 job 队列(`std::sync::mpsc`)。
- **job**:`BatchJob { kind, pcm: Arc<Vec<i16>>, sr }`,
  - `kind = Sentence { paragraph_id, sentence_id }` | `Paragraph { paragraph_id }`。
- **执行**:worker 取 job → `batch_asr.recognize(&pcm, sr)`(**带超时**,默认 30s,超时任
  `None`)→ 发结果事件。**每个 job 必然产出一个结果事件**(`Some` / `None`),不丢不堵,
  保证 §5.6 的就绪计数一定能到齐。
- **PCM 共享**:job 携带 `Arc<Vec<i16>>`,与 AudioStore / VadParagraph 共享,避免拷贝
  (需把 `AudioStore` 由 `BTreeMap<u64, Vec<i16>>` 改为 `BTreeMap<u64, Arc<Vec<i16>>>`,
  或在 job 里按 `audio_id` 从 store 取 —— 二选一,推荐前者)。
- **生命周期**:由 `Pipeline::run` 启动一次,job 通道关闭时 worker 排空退出;`batch_asr`
  以 `Arc<dyn AsrProvider>` clone 注入。

> **为什么是池而不是单线程(FIFO)**:见 §6 的 B1/B2 对比。句级 batch ~3.5s 与真实语速
> (每 3–8s 一句)同量级,单线程会在连续说话时**积压**,定稿延迟被拉大;N=2 提供 2× 吞吐
> (每 ~1.75s 一句),舒适地覆盖典型语速,同时段级重跑(长 job,整段音频)占一个 worker,
> 句级短 job 用另一个,互不拖累。

### 5.3 新事件模型(`Stage1Event` 增补 2 个,保留 3 个)

`lib.rs` 的 `Stage1Event` 是 **Stage1 → pipeline 的内部契约**(非 wire,可改)。
新增:

| 事件 | 载荷 | 语义 |
|---|---|---|
| `StreamFragment`(不变) | pid, sid, text, at_s | 流式 partial + 句定稿流式,inline 直发前端 |
| `Batch`(语义微调) | pid, `sentences` | 每句 EOS 发出;**新句 `batch_text` 恒 `None`**(batch 异步)。仍带"当前段落全部句"(streaming 已定、batch 待定) |
| `SentenceBatchReady`(**新增**) | pid, sid, `batch_text: Option<String>`, `asr_ms: u64` | 某句 batch 完成 |
| `ParagraphEdge`(语义微调) | `paragraph: VadParagraph` | 段落边界关闭;`paragraph.batch_text` 恒 `None`(整段重跑异步);`pcm`(Arc)仍在,供归档/重跑 |
| `ParagraphBatchReady`(**新增**) | pid, `batch_text: Option<String>`, `asr_ms: u64` | 整段重跑完成 |

**wire 不受影响**:`AsrEvent`(`aura-agent/view.rs`)的 5 个 tag/字段名 **FROZEN**。
pipeline 把新内部事件映射回**同名**的 `TurnEvent` / `AsrEvent`(§5.4),只是**时序**变了
(`BatchSentence` / `BatchParagraph` 晚到)。前端 `voice_state.rs` 按 id 折叠、关闭快照稳定,
**零改动**(§7 验证)。

### 5.4 各组件流程

**消费循环(`recognizer.rs`,不再阻塞)**

- `finalize_sentence`(EOS):
  1. `streaming_text = 流式 finalize`(不变,快)。
  2. 建 `VadSentence { batch_text: None, .. }`,PCM 入 store(**不再调 `recognize`**)。
  3. `tracker.on_eos` → 段落决策(不变)。
  4. **入队句级 job**(克隆 `Arc<pcm>` 到 `BatchPool`,非阻塞)—— 替代 `recognize`。
  5. 若大 gap 先定稿上段 → `emit_paragraph_edge`(见下)。
  6. `emit StreamFragment(定稿流式) + Batch { pid, sentences }`(sentences 里新句 batch=None)。
- `emit_paragraph_edge`(settle):
  1. `store.concat(&ids)` 拼出段落 `Arc<pcm>`(不变,廉价)。
  2. 建 `VadParagraph { batch_text: None, pcm, .. }`(**不再调 `recognize` 重跑**)。
  3. **入队段级重跑 job**(携 `Arc<pcm>`,非阻塞)—— 替代重跑。
  4. `emit ParagraphEdge { paragraph }`。
  5. `store.evict(&ids)`(不变)。
- **消费循环自始至终不等待任何 batch 结果** —— 这就是 bug 的根除点。`speaking` 由持续
  喂帧的流式会话维持真实值,`check_settle` 的墙钟判定在"有语音在 ring 里"时被正确抑制。

**batch 池(`batch_pool.rs`)**

- worker 线程循环:取 job → `recognize`(带 30s 超时)→ `send(SentenceBatchReady |
  ParagraphBatchReady { pid, sid?, text: Option, asr_ms })`。
- 结果发往 pipeline 的 Stage2 输入通道(与消费循环的 `Batch` / `ParagraphEdge` **同一通道**,
  多 sender 安全,§5.8 通道拓扑)。

**pipeline / Stage2 worker(`pipeline.rs` + `calibrator.rs`)**

事件在同一条 mpsc 上**有序到达**(但 `Batch` 与 `SentenceBatchReady` 的**交叉**是任意的,
§5.7 已证明任意交叉都安全)。处理:

| 内部事件 | pipeline 动作 | 产出 `TurnEvent` |
|---|---|---|
| `StreamFragment` | inline 直发(不变) | `StreamFragment` |
| `Batch { pid, sentences }` | ① 累积 `sentences[pid]`(设 streaming/时序,**保留已有 batch**);② `s2.calibrate_paragraph(pid, &sentences[pid])`(best_text,batch 缺 → 流式回退)→ live 预览 | `SentenceCalibration` |
| `SentenceBatchReady { pid, sid, text }` | `sentences[pid][sid].batch_text = text` | `BatchSentence`(前端该句 batch 文本,晚到) |
| `ParagraphEdge { paragraph }` | 存段落(含 `Arc<pcm>`),记 `expected = paragraph.sentences.len()`,标 pending | (无,内部) |
| `ParagraphBatchReady { pid, text }` | 设段落 `batch_text = text`;若**就绪**(§5.6)→ 定稿 | `BatchParagraph` + `ParagraphCalibration` |

**定稿动作**(就绪时,pipeline 内):
1. `final = s2.finalize_paragraph(&段落)` —— **现改为跑一次 LLM**(用全句 best_text,此时
   batch 已齐),替代旧"零 LLM 取存档"。
2. `on_turn(TurnEvent::BatchParagraph { pid, text: 段落 batch_text 或拼接回退 })`。
3. `storage.record_final(FinalTurn { pid, raw_text: 段落 batch_text, streaming_text,
   calibrated: final, pcm, .. })` —— **归档从旧的 `ParagraphEdge` 臂移到定稿臂**(此时
   `batch_text` 才齐)。
4. `on_turn(TurnEvent::ParagraphCalibration { pid, calibrated: final })`。
5. 清理 `sentences[pid]`(段落结束)。

> 单句段落:现状 `emit_paragraph_edge` 已"免重跑"(复用句级 batch)。异步化后,单句段落的
> 段级重跑 job **不投递**(直接复用句级 `SentenceBatchReady`),`ParagraphBatchReady` 由
> pipeline 在句级就绪时合成 —— 保持"大多数段落省一次 batch"的优化。

### 5.5 Stage2 状态机改动(`calibrator.rs`)

- **去状态化**:删除 `current: Option<(ParagraphId, String)>`(旧"存最后一次整流,定稿取
  存档")。原因:异步下"最后一个 Batch 已整流完全段"的不变式不再成立(末句 batch 可能
  未就绪),定稿必须**自己跑一次 LLM** 拿最佳文本。
- `calibrate_paragraph(pid, sentences)`:**不变**(live,每 Batch 一次,best_text)。
- `finalize_paragraph(paragraph)`:**改为跑 LLM**(全句 best_text)。语义从"移动左边界、
  零 LLM"变为"用最终最佳文本整流一次"。失败回退原文(沿用 `joint_calibrate` 的
  `unwrap_or_else`)。
- `LlmInput`(batch/stream/both)语义不变;`finalize` 按 `input` 选源(与 `calibrate` 一致)。
- `PassThroughCalibrator`(LLM 禁用):`finalize` 仍返回 `paragraph.best_text()`(零 LLM),
  行为不变。

### 5.6 定稿触发(readiness-based)

定稿 = **段级重跑完成** **且** **全句 batch 就绪**(或超时兜底)。事件驱动,无需显式等待:

- `ParagraphEdge` → `pending[pid] = { expected, ready: 0, para_done: false }`。
- `SentenceBatchReady { pid, sid }` → `pending[pid].ready += 1`(每句恰好一次,§5.2 保证)。
- `ParagraphBatchReady { pid }` → `pending[pid].para_done = true`。
- 任一步后检查:`para_done && ready == expected` → 定稿。
- **超时兜底**(防 job 异常丢失):`ParagraphEdge` 记 deadline(默认 15s);pipeline 用一个
  轻量 tick(后台线程每 ~1s 发 `Tick` 事件)扫描 `pending`,超期则按"当前最佳文本"定稿
  (重跑/缺句 batch 按 `None` 回退)。`BatchPool` 的"每 job 必出结果 + 30s 超时"已把此兜底
  变成极端防御。

> **为什么需要 `ready == expected` 而非只等 `para_done`**:末句 batch 可能比段级重跑**晚**
> (池并行下重跑可能先完成)。定稿要用**全句 best_text**,必须等末句 batch 到齐,否则末句
> 退化成流式(质量回退)。这是异步化下"定稿用最佳文本"正确性的关键。

### 5.7 顺序保证(invariants)

需保证:**一个段落的 `BatchSentence`×N 与 `ParagraphCalibration` 的最终语义正确**。逐条:

1. **同一句的 `Batch` 先于其 `SentenceBatchReady`**:`Batch` 在 EOS 由消费循环发出,
   `SentenceBatchReady` 在 batch 完成后由池发出(更晚)。✓
2. **`SentenceBatchReady` 之间任意顺序都安全**:pipeline 按 `sid` 累积(`sentences[pid][sid]`),
   前端按 `(pid, sid)` 折叠(`upsert_sentence`),顺序无关。✓
3. **`BatchParagraph` 可能先于部分 `BatchSentence`**(池并行下重跑先完成):前端在
   `BatchParagraph` 标 `closed`;晚到的 `BatchSentence` 更新已关闭段落的句 batch 并重算预览 ——
   关闭段落的 `plain_preview` 优先 `batch_paragraph`(voice_state.rs:88-97),**不受影响**。
   定稿(§5.6)本就要等 `ready == expected`,所以 `ParagraphCalibration` 必然在所有
   `BatchSentence` 之后。✓
4. **跨段落**:`ParagraphEdge` 后才开新段落;旧段落的重跑/定稿事件按 `pid` 隔离,
   不污染 `current_paragraph` 的预览。✓
5. **`StreamFragment` 恒 inline**(不经池),live 显示不受 batch 延迟影响。✓

### 5.8 数据结构与通道拓扑

**pipeline 新增状态**(Stage2 worker 内,单线程访问,无锁):

```rust
// 每段累积的句(流式来自 Batch,batch 来自 SentenceBatchReady)
sentences: HashMap<ParagraphId, Vec<VadSentence>>,
// 定稿就绪表
pending:   HashMap<ParagraphId, PendingFinal>,
struct PendingFinal { expected: usize, ready: usize, para_done: bool,
                      paragraph: VadParagraph, deadline: Instant }
```

**通道拓扑**(多 sender → 单 receiver):

```
消费循环 ──┐ (StreamFragment 走 on_turn inline,不入此通道)
           ├─► mpsc::Sender<Stage1Event> ──► Stage2 worker (mpsc::Receiver)
batch 池  ──┘   (Batch / ParagraphEdge /
                 SentenceBatchReady / ParagraphBatchReady / Tick)
```

`mpsc::Sender` 可 Clone + 多 sender,消费循环与 batch 池各持一个 clone,Stage2 worker
单 receiver 顺序消费。`StreamFragment` 保持 inline(高频,不占 Stage2 通道)。

**`AudioStore` 改动(可选但推荐)**:`Vec<i16>` → `Arc<Vec<i16>>`,让 job / 段落 / store
共享 PCM。若不改,job 按 `audio_id` 从 store 取(多一次 `clone`,可接受)。

---

## 6. 备选方案与权衡

| 方案 | 描述 | 评价 |
|---|---|---|
| **A 对症**(不推荐) | `speaking` 额外看 ring 是否非空(阻塞期间有积压语音 → 抑制定稿) | 几行止血,但**没解决吞吐/延迟**:batch 仍卡消费循环,流式 partial 在 batch 期间冻结,长段落延迟依旧。治标 |
| **B1 单线程 FIFO 池** | 一个 batch worker,job 严格 FIFO | 顺序天然保证(重跑必在末句之后),最简。但**吞吐 = 1 句/3.5s**,连续说话时积压,定稿延迟被拉大(3 句段可达 ~14s)。**不选为主方案**,作 N=1 配置 |
| **B2 池 + readiness 定稿**(本文主方案) | N=2 池并行 + 就绪计数定稿 | 吞吐 2×,重跑(长 job)与句级(短 job)并行;定稿延迟 ≈ max(末句 batch, 重跑),**不劣于现状**(现状句级 batch 本就在说话间隙跑)。代价:pipeline 多 ~40 行状态 + 去状态化 Stage2 |
| **batch 挂 Stage2 线程**(拒绝) | 把 batch 挪到 `aura-stage2`(已独立于消费循环) | 最省(无新组件),但 **batch 与 LLM 串行**在同一线程:live 校准被 batch 延迟 ~3.5s(UX 回退),且 LLM + batch 互相拖累。**拒绝** |
| **去掉段级重跑**(拒绝) | 段落文本 = 句级 batch 拼接,省一次重跑 | 损失"整段跨句重听"的权威质量(现状明确要它);属另一优化维度,非本 bug 所需 |

**主方案 = B2**。理由:唯一同时满足"根除阻塞(目标 1)+ 吞吐不降(目标 2)+ 定稿最佳文本
(目标 4)"的方案;改动收敛在 `aura-core`(目标 3)。

---

## 7. 边界与失败模式

| 场景 | 行为 | 说明 |
|---|---|---|
| **batch 网络失败** | `recognize` Err → 结果 `None` → 句/段 `batch_text=None` → best_text 回退流式 | 与现状完全一致(`Option` 回退是既有契约) |
| **batch 超时**(>30s) | worker 判超时 → 发 `None` → 同上回退 | 新增:给 batch 一个独立超时(区别于转发的 300s),防单 job 永久占 worker |
| **重跑 job 异常丢失** | `ready`/`para_done` 到不齐 → tick 超时(15s)按当前最佳文本定稿 | 极端防御;`BatchPool`"每 job 必出结果"已使其几乎不可达 |
| **idle 深度睡眠**(`running=false`) | 消费循环退出;batch 池**继续排空**在途 job(发结果到 Stage2) | 池独立于 `s1.run`,不受 idle 影响;结果对应已暂停的段落,前端折叠无害 |
| **重连 / `reset` / `sync_history`** | 前端清空;在途的旧段落 `*Ready` 事件到达 → 更新已无关的 `pid` 状态 | 按 `pid` 隔离,不污染当前预览;无害(现状重连也有同类残留) |
| **flush_paragraph**(IME"我说完了") | 跳过 merge_gap,立即入队重跑 job + `ParagraphEdge`;消费循环不阻塞 | 与非 flush 路径同构;定稿由 readiness 驱动 |
| **batch 禁用**(`asr.backend: disable`) | `DisabledAsr` 恒返空 → 结果 `None` → 全流式 | 与现状一致;`BatchPool` 仍跑(空识别,廉价),或可特判跳过(优化) |
| **单句段落** | 不投重跑 job,复用句级 `SentenceBatchReady` 合成 `ParagraphBatchReady` | 保持"大多数段落省一次 batch" |

---

## 8. 成本分析

- **LLM 调用**:现状 N(每 Batch)+ 0(定稿零 LLM)= **N** 次/段。
  异步化:N(每 Batch live)+ **1**(定稿跑 LLM)= **N+1** 次/段。
  **增量 = 每段 1 次**(定稿那次,用来把末句 batch 纳入最终文本)。
  注:现状 Stage2 本就 O(n²)(每次 `calibrate` 整流全段),+1 可忽略。
- **内存**:`AudioStore` 改 `Arc` 后,batch job / 段落 / store 共享 PCM,**不新增拷贝**;
  池在途 job 持 `Arc` 指针(非 PCM 本体)。
- **定稿延迟**:≈ `max(末句 batch, 段级重跑)`。现状句级 batch 在说话间隙同步跑(阻塞循环),
  定稿只需等重跑;异步化句级 batch 并行跑,**定稿延迟不劣于现状**,且 live 显示不再冻结。
  段级重跑(长段落 ~10s)的固有延迟**保留**(属重跑本身的成本,非本设计引入;见 §10 后续)。
- **线程**:新增 N=2 个 std 线程(常驻,空闲时 park 在 job 通道上,零 CPU)。

---

## 9. 迁移与测试计划

**改动面(全部在 `aura-core`)**:

| 文件 | 改动 |
|---|---|
| `lib.rs` | `Stage1Event` 增 `SentenceBatchReady` / `ParagraphBatchReady`;`Batch` / `ParagraphEdge` 语义注释 |
| `batch_pool.rs` | **新增** `BatchPool`(N worker + job 通道 + 30s 超时 + 必出结果) |
| `recognizer.rs` | `finalize_sentence` / `emit_paragraph_edge` 改为**入队 job** 替代 `recognize`;持有 `BatchPool` sender;删同步调用 |
| `pipeline.rs` | Stage2 worker 处理新事件 + `sentences` / `pending` 累积 + readiness 定稿 + `record_final` 移到定稿臂;启动 `BatchPool`;tick 线程 |
| `calibrator.rs` | `finalize_paragraph` 改跑 LLM;删 `current` 状态;`PassThrough` 适配 |
| `audio_store.rs` | (推荐)`Vec<i16>` → `Arc<Vec<i16>>`(job/段落/store 共享 PCM) |

**job 通道所有权(接线)**:job 通道在 `assemble` 创建 —— **sender 存进 `OnnxStage1Recognizer`**
(消费循环 EOS/settle 入队用),**receiver 留在 `Pipeline`**;`Pipeline::run` 启动 batch 池
worker 时把 receiver + `batch_asr` 的 clone + Stage2 `tx` 的 clone 注入 worker。消费循环
(`s1.run`)与 batch 池分属两线程却共用一条 job 通道,结果汇入同一条 Stage2 输入通道。

**前端 / daemon / wire:零改动**(`AsrEvent` FROZEN;`voice_state.rs` 按 id 折叠天然兼容晚到事件)。

**测试**:

1. **单元**(`batch_pool`):每 job 必出结果(成功 / 失败 / 超时三态);多 job 并发不串。
2. **单元**(`recognizer` 边界):EOS 入队 + 不阻塞(用 mock `AsrProvider` 睡眠 5s,断言消费
   循环在期间仍能处理下一句 / 喂流式)。
3. **集成**(关键回归):**复现原 bug 场景** —— 句 1 EOS 后 batch 故意慢(> merge_gap),
   断言第二句**并入同一段**(不吞第一句);并断言定稿文本 = 两句 batch 拼接。
4. **集成**(顺序):构造"重跑先于末句 batch 完成",断言 `ParagraphCalibration` 仍在所有
   `BatchSentence` 之后、且用末句 batch(非流式回退)。
5. **端到端**(手动):真机 1–3.5s 停顿连说,观察 fcitx 预览不再吞句;`/api/asr_stream`
   事件时序符合 §5.7。
6. **回归**:`cargo test -p aura-core`(feature `asr`)+ 现有 `calibrator` / `pipeline` /
   `voice_state` 测试全绿(Stage2 去状态化后 `paragraph_state_machine_overwrites_and_
   finalizes_without_llm` 需按新语义改写:定稿**有** 1 次 LLM)。

**灰度**:`BatchPool` 的 N 可配(0 = 退回现状同步路径,作开关回退);上线先 N=2,
观察定稿延迟 / LLM 成本后再调。

---

## 10. 开放问题 / 后续(本轮不做)

1. **段级重跑延迟**(长段落 ~10s):可考虑"短段落跳过重跑(阈值,如 <3s 音频)"或
   "重跑结果作为**增强**而非定稿前置(先按句级 batch 定稿,重跑到达后刷新归档)"。
   后者让定稿更快,但引入"定稿后文本被刷新"的语义,需前端配合 —— 单独设计。
2. **live 校准增强**:可选在 `SentenceBatchReady` 触发再校准(based-on-batch 的 live 预览),
   代价是每句 +1 次 LLM(O(n²) 放大)。默认关,作可配开关。
3. **定稿 LLM 省一次**:若"最后一次 `calibrate` 时全句 batch 已齐"(罕见),可复用其结果、
   省掉定稿那次 LLM —— 需 Stage2 记忆最后一次校准的输入完整性,复杂度换一次调用,暂不做。
4. **batch 池自适应 N**:按在途 job 数 / 延迟动态调 N。
5. **`AudioStore` 的 `Arc` 化**影响面评估(其它持有 PCM 的路径:归档 WAV、`/api/audio`)。

---

## 附:与现状的关键差异(一页速览)

| 维度 | 现状 | 异步化(B2) |
|---|---|---|
| 句级 batch | 消费循环**同步**(~3.5s 阻塞) | 池**异步**,消费循环不阻塞 |
| 段级重跑 | 消费循环**同步**(settle 时阻塞) | 池**异步**,settle 仅入队 |
| 消费循环 | 被 batch 卡 → `check_settle` 误切(**bug**) | 持续运行,`speaking` 真实 → 不误切 |
| live 流式显示 | batch 期间**冻结** | 持续流动 |
| Stage2 | 有状态(存最后一次整流),定稿**零 LLM** | 无状态,定稿**跑 1 次 LLM**(纳入末句 batch) |
| LLM 调用/段 | N | N+1 |
| 定稿延迟 | 等重跑(句级已同步跑完) | ≈ max(末句 batch, 重跑),不劣于现状 |
| 前端 / wire | — | **零改动**(`AsrEvent` FROZEN) |
| 改动收敛 | — | 仅 `aura-core`(Stage1 + pipeline + Stage2) |
