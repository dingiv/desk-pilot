# aura 后端流水线执行与事件触发状态流(as-built + 前后端时序错位清单)

> **状态:现状实录(2026-09-01);round13 修复 + round14 线程模型收拢已落地(见下)。** round12(tokio 化)+
> round11 S3 之后,实测**前后端时序对接错位、不可用**——本文是修错前的地基文档:后端
> 流水线**实际怎么执行**、每个事件**由什么状态触发**、在哪个线程上**何时发出**,以及
> 据此核对出的**错位清单(§7)**。
>
> **round13 修复记录(2026-09-01,同日落地;代码为准)**:
> - **A → 已修**:段落 id 改为**创建时刻时间戳**(UNIX_EPOCH 微秒,严格递增
>   `max(now, last+1)`)—— recognizer `next_win_id`(取代 `next_random_win_id`);
>   id 即顺序对客户端成立,lib.rs 契约文档同步。
> - **B → 已修**:**起音即开段**—— VAD detected() rising edge 即分配真实段落 id
>   (`on_speech_onset`,feed_streaming 起音臂调用),live partial 从第一条起携带真键,
>   幽灵段根除;配套**空段 GC**(开段后从未出句,静默满 merge_gap 静默丢弃,防陈旧
>   空段被远期语音复用导致 id 错位)。回溯 SOS 降级为防御兜底。
> - **D → 已修**:主循环只为 **just-closed 句**(Batch 载荷最后一个)投句任务,N²
>   重投回归消除。
> - **E.3 → 已修**:**live 整流回归 Batch 臂**——每 Batch 触发一次 live 校准
>   (`live_calibration_task`,段内链式串行,SC 顺序 = 段落生长序);段任务只做
>   join 回填 + 段重跑 + 定稿,并在定稿前等 live 链尾(全部 SC 先于 PCal)。
>   架构需求"1s 空白 → Batch 识别 → stage2 紧跟纠偏,先后明确"恢复成立。
> - 测试:audio-aura-core 65+1 全绿(新增 tracker 4 + live 链 1,改写段任务 2)。
> - **round14 线程模型收拢(2026-09-01)**:`Pipeline::run` 改为**纯 async future** ——
>   run() 内部不再声明任何 std 线程;阻塞桥经 `spawn_blocking` 骑 runtime blocking
>   pool,主循环即 run() 自身。daemon 把 `pipeline.run(..)` 直接 spawn 到 socket
>   runtime(**零专用线程**);examples/bench 用 `Pipeline::spawn`(一条专用线程 +
>   current_thread rt)便捷入口。事件语义/顺序不变,纯执行载体简化。
> - **round14b 消费循环 async 化(同日)**:阻塞桥 ②(s1 消费循环)从 spawn_blocking
>   改为**原生异步任务** —— 帧等待从 `Condvar` 换 `tokio::sync::Notify`(permit 语义
>   防丢唤醒),VAD(32ms/帧,微秒级)在 executor 上,流式解码 round21 起独立任务;
>   深度睡眠/恢复从 `(Mutex, Condvar)` 换 `Arc<Notify>`(daemon resume 同步侧
>   `notify_one` 即可)。**唯一剩余阻塞桥 = scout TCP ingest(sync IO)**。
>   trait 方法改 RPITIT + `Send`(async fn in trait 写不出 auto bound)。冒烟实测
>   idle 深睡 → 客户端接入 resume 闭环正常(见 §1 拓扑表)。
> - **round16 统一发射留痕 + round16b 定稿回填修复**:主循环(单点发射)每条发往
>   前端的 TurnEvent 先记一条 `info`(describe_turn 单行摘要,p 值即时间戳可比大小)
>   —— 前后端时序对表的权威序列。日志随即钉死一个 round12 回归:**paragraph_task
>   的 join 回填结果(acc)从未写回段落实体** —— 定稿/归档用的是 ParagraphEdge
>   快照(各句 batch_text 恒 None,tracker 副本从不更新)→ best_text 静默回退流式;
>   PassThrough(LLM 禁用)下 PCal 直接发流式拼接,经 REPLACED 把 finals 已到手的
>   batch 占位换回流式(单句段落无 BP 掩盖,完全裸露;多句段落被段重跑掩盖)。
>   修复:回填写回 `paragraph.sentences`;主循环另维护**已回填**句集(Batch 刷新 +
>   BS patch),live SC 的输入同样吃到前句 batch。回归测试
>   `paragraph_task_finalize_uses_backfilled_sentence_batches` 钉死。
> - **round17 panic 修复 + round17b SC 触发点修正**:① 段重跑/归档的阻塞调用裸跑在
>   async 任务里 → `reqwest::blocking` 内部 runtime drop **panic** → 段任务崩死、
>   PCal/归档永久丢失(实测 p724)—— 重跑包 `spawn_blocking` + 15s 兜底超时
>   (**PCal 必发**成为硬保证),归档同包;② live SC 触发点从 Batch 事件(EOS 时刻)
>   **移到该句 BS 到达时** —— 架构要求"batch 完成 → 之后纠偏,先后明确"(此前 SC
>   抢在 BS 前、内容退化为流式);③ Batch 臂句集合并(round16b 的编辑当时静默
>   未生效,本次落定):保留 BS 已回填的 batch,SC/PCal 输入不再退回全流式。
> - **round17c 纠偏输入双通道(默认)**:`LlmInput` 默认 `batch` → **`both`** —— 纠偏
>   纠的就是 batch + 流式两路识别的结果,参数必须都传进 Stage2(prompt 双转写对照:
>   `<primary>` = best_text(batch 优先),`<secondary>` = 流式拼接,批式丢句首由流式
>   补回)。单路(batch/stream)降为显式配置的降级模式;aura.yaml 同步 `input: both`。
> - **round18/19 前端留痕 + 丢事件三连修**:① 接收侧统一留痕(`前端←event`,与 server
>   `emit→前端` 同词汇,两边 diff 即定位缺口);② **SSE 30s 掐流根因** —— `AuraClient`
>   的 `.timeout(30s)` 覆盖整个响应生命周期,长流每 ~30s 被掐(重连窗口事件永久丢 +
>   UI 闪"不可用")—— 拆出**无总超时**的 SSE 专用 client(仅连接超时);③ daemon 的
>   `{"type":"hello"}` 握手 ack 静默跳过(曾误报"契约不匹配");④ SSE 内部重连成功后
>   自发 `Resync`:reset + `/api/results` 全量对账,补断连窗口丢的定稿(广播无回放,
>   这是唯一补历史通道)。
> - **round20 SC 陈旧遮蔽修复**:`cascade_preview` 的"纠偏 > batch > 流式"级联中,
>   SC 是**快照**—— 只覆盖到触发它的那句 BS,**不知道自己已过时**:同段第二句
>   流式期间,best_calc 恒返回陈旧 SC,新句被遮住(实测"第二句不刷新 UI")。
>   修:`ParaState` 记 SC 覆盖上界(`sc_covers_sid` = 触发它的 BS 句 id),
>   过界新句以 batch>流式 **续接**在 SC 之后 —— SC 仍优先,但只顶替它覆盖的部分。
> - **round20b 覆盖上界走协议**:`segment_calibration` 事件**自带 `segment_id`**
>   (触发该次纠偏的 BS 句)—— 客户端的派生记账(max_bs_sid 追踪)删除,fold
>   直接取事件字段。SC"该覆盖谁"由 wire 契约声明,不再前端推断。
> - **round21 流式模型独立任务**:accept/decode(ONNX 前向)从消费循环拎出 ——
>   独立 tokio::task(**async fn**,`tokio::spawn` 交 executor 协作调度,不占阻塞
>   线程)owns 流式 session + 节流解码,消费循环只转发帧指令(`Onset`/`Feed`/
>   `Reset`/`Finalize`)。**VAD/分句/段落定稿从此与流式推理零共享执行流**;partial
>   回传后仍由消费循环发射(两任务汇于同一事件出口,SF→BS→PC/PCal 全序不破)。
>   EOS 定稿 = 每句一次、**回执同通道**(`StreamOut::Finalized`,round24 起不再有
>   per-句 oneshot —— 整个流式任务只有一对通道)。B 侧 last_partial 状态以
>   `PartialMirror`(nonempty + last_change)镜像进消费循环,供 speaking 抑制 /
>   断流喂静音 / 停滞看门狗;重置/定稿点由消费循环直接清零(确定性,无竞态)。
>
> - **round24 R4 + channel 简化**:① `select!` 两臂臂体拆出为 `on_stage1_batch` /
>   `on_stage1_paragraph_edge` / `on_turn_batch_sentence` 处理器(共享依赖收进
>   `Ctx`,可变账本收进 `Turns`),主循环只剩分派 + 单点 emit;② 流式任务通道
>   简化为一对(cmd/out)—— EOS 定稿回执走 out 通道的 `Finalized` 变体
>   (`await_finalize` 挂起等回执,途中 partial 依序先发),删掉 per-句 oneshot。
>
> 本文取代原"Stage1 batch 异步化设计"(该设计已落地并被 round12 取代,历史内容见 git)。
> 代码为准。行号以当前工作区为准:
> Stage1 编排 = `crates/aura-core/src/pipeline/`(round23 文件夹化:mod = 编排汇点,
> consume = 消费循环,recognizer = 资源/配置,tasks = batch/纠偏任务壳,tracker = 边界
> 数学,stream = 流式任务,calibrator = Stage2);采音 + VAD 检测 = `pipeline/vad.rs`
> 〔`ingest_loop` + `VadFront`〕。编排入口 = `crates/aura-core/src/pipeline/mod.rs`,
> 契约 = `crates/aura-core/src/lib.rs`,daemon = `apps/audio-aura/src/main.rs`,
> wire = `crates/aura-agent/src/view.rs`,客户端折叠 = `crates/aura-agent/src/transcript.rs`。

---

## 0. 全链路总览

```
麦克风 ──► omni-scout(:7878 /audio TCP)
        ──► [blocking pool]       scout → AudioRing(Notify 唤醒;spawn_blocking,sync IO)
        ──► [异步任务]            消费循环:VAD 门控流式 + 分句 + 段落决策(原生 async)
                                      │ Stage1Event(5 种,FIFO)
                                      ▼ s1_tx (tokio unbounded)
              [pipeline.run 任务]   select! 主循环 —— on_turn 唯一调用者(单点发射)
                                     (round14:daemon = rt 任务,零专用线程)
                  ├─ StreamFragment ──────────► 直发
                  ├─ ParagraphClosed ─────────► 直发(边界,先于下一段任何事件)
                  ├─ Batch ──► 句任务 ×N(spawn_blocking recognize_once)
                  │               └─► BatchSentence ──► turn_tx ──► 主循环 emit
                  └─ ParagraphEdge ──► 段任务(join 句任务 → live 整流 → 段重跑
                                          → 定稿整流 → 归档)
                                      └─► SentenceCalibration / BatchParagraph /
                                          ParagraphCalibration ──► turn_tx ──► emit
                                      ▼
              daemon on_turn:TurnEvent → AsrEvent(wire tag FROZEN)
                                      ▼ broadcast(1024)
              GET /api/asr_stream(SSE,data: {json}\n\n)
                                      ▼
              [aura-agent client] 字节级分帧 → parse_event
                                      ▼ fold_event
              SharedTranscript(折叠状态机)──► IME 候选/预览
```

---

## 1. 执行拓扑(谁在哪跑什么)

| 线程 / 任务 | runtime | 运行内容 | 产出 |
|---|---|---|---|
| blocking pool(ingest) | tokio blocking | `s1.run_ingest()`:scout TCP → AudioRing,自动重连(2s 退避) | ring + Notify(permit) |
| 消费循环任务(round14b) | **原生异步** | `s1.run(cb).await`:帧等待 = Notify + 截止;VAD/流式内联;深睡 = 等 resume Notify | `Stage1Event` → `s1_tx` |
| `pipeline.run` future | daemon rt 任务 / 独立宿主专用线程 | `select!` 主循环:两源(s1_rx / turn_rx)单点 `on_turn` | `TurnEvent`(wire 前形态) |
| 句任务 | spawn_blocking | `recognize_once(句 PCM)`,完成即回传 | `BatchSentence` → `turn_tx` |
| 段任务 | tokio::spawn | join 句任务(就绪门)→ live 整流 → 段重跑 → 定稿 → 归档 | §3.3 四种事件 → `turn_tx` |
| `aura-pipeline`(外层) | std | `spawn` 包装 `run`(满足 `-> !`);daemon 用 | — |
| `aura-socket` | multi_thread tokio | daemon 主线程:axum SSE、控制面、idle 监控 | broadcast `AsrEvent` |

**通道**:

| 通道 | 类型 | 语义 |
|---|---|---|
| ring + condvar | `Mutex<AudioRing>` + Condvar | ingest → 消费循环;`wait_frame` 支持截止时间(无轮询) |
| `s1_tx` | tokio unbounded mpsc | 消费循环 → 主循环;`Stage1Event` **FIFO = VAD 顺序** |
| `turn_tx` | tokio unbounded mpsc | 句/段任务 → 主循环;完成序,与 s1_rx 任意交错 |
| `asr_events` | tokio broadcast(1024) | daemon → SSE 订阅者;lagged → comment 帧(客户端 warn 感知) |

**深度睡眠**:`running=false` → 消费循环退出(condvar 等 resume);ingest 照常;idle 由
daemon 无订阅超时触发。暂停期间**无任何事件**。

---

## 2. Stage1 消费循环(`aura-stage1` 线程)—— 事件触发的第一现场

### 2.1 帧循环(每 32ms 一帧,`run()` recognizer.rs:1063)

```
⓪ running=false?            → return(深度睡眠)
① active=false?             → park 等音频(scout 暂停),continue
② 时间驱动检查(now_s = 墙钟):
   speaking = 流式 partial 非空          ← 回溯式 VAD 的关键抑制量
   ②a flush_paragraph && !speaking       → force_settle → ParagraphEdge(§2.3-c)
   ②b tracker.check_settle(now, speaking)→ ParagraphEdge(§2.3-a)
   ②c 流式会话停滞 ≥ 8s 且 partial 未变   → 重置会话(防幻觉残留)
   ②d 诊断日志(3s)
③ 取帧:ring 有帧直接取;空则 park 到最早截止
   (settle deadline / flush 50ms / 看门狗 / 断流静音喂);
   断流 > 2s 且有 partial → 喂合成静音逼 VAD EOS
④ VAD:push_frame → v_detected() + events(SOS/EOS)
⑤ 流式(VAD 门控):
   detected 翻转(false→true)→ 补喂 lead-in(~0.5s,soft onset)
   每 15 帧(≈0.5s)解码一次 partial,变化才发:
       StreamFragment{paragraph_id=prospective(), sentence_id=prospective()}   ★ §7-B
   空闲 → 帧进 lead-in 环形缓冲(有界)
⑥ 分句事件:
   SOS → cur_sentence = tracker.on_sos()      ← 回溯式:与 EOS 同批到达!
   EOS → finalize_sentence(§2.2)
```

### 2.2 `finalize_sentence`(EOS 臂,recognizer.rs:935)—— 句事件发射序

1. 句 PCM = 流式会话累积的完整音频(含 soft onset;流式与 batch 同源);
2. `start_s` 由 PCM 时长**回溯**(SOS 墙钟 = EOS 瞬间,不可用);
3. `VadSentence{batch_text: None}`(batch 异步,in-flight);PCM 入 AudioStore;
4. `tracker.on_eos(sentence)` → `(settled?, paragraph_id, 全部句)`;
   - settled = 大 gap 在此处关上段(§2.3-b)→ **先** `emit_paragraph_edge(prev)`;
5. 发**句定稿流式** `StreamFragment`(该句权威流式文本);
6. 发 `Batch{paragraph_id, sentences=当前段全部句, sr}`;
7. `batch_jobs=false` → 不投 job(round12:句任务由主循环在 Batch 臂自建)。

**同一 EOS 内发射序(s1_tx FIFO 保证)**:
`[ParagraphEdge(prev段)?] → StreamFragment(final) → Batch`。

### 2.3 ParagraphTracker 状态机(段落决策,纯逻辑)

状态:`open: Option<OpenParagraph{paragraph_id, sentences, active}>`。

**关键事实(修复前):SOS 是回溯的**——Silero 只在句完成时弹出 SOS+EOS 对
(recognizer.rs:604 注释)。**round13 后:段落起音即开**(detected() rising edge →
`on_speech_onset` 分配时间戳真 id),partial 从第一条起携带真键;回溯 SOS 只补
sentence id(开段降级为防御兜底):
- ~~live partial 用 `prospective()` 预测键~~ → 现返回**开段真键**;
- 段落 id 分配 = `next_win_id()`(recognizer.rs)——**时间戳微秒,严格递增**
  (`max(now, last+1)` 防时钟回拨),id 即顺序;

**三条关段路径**(都产出 `ParagraphEdge` → `emit_paragraph_edge`,recognizer.rs:733):

| 路径 | 触发 | 时点 | 抑制条件 |
|---|---|---|---|
| a. `check_settle`(661) | 每帧循环②b,墙钟 `now - last.end_s ≥ merge_gap` | **及时**(静默满即关,唤醒由 settle_deadline 驱动) | `active`(句进行中)或 `speaking`(partial 非空——下一句已在说但 SOS 未到) |
| b. `settle_if_gap`(624) | 下一句 EOS 时,回溯 onset 与上句 end 的 gap ≥ merge_gap | **迟到**(等下一句 EOS) | **几乎不可达的防御路径**:回溯 onset 含 lead-in(比起音更早),若 a 在 deadline 被 speaking 抑制,则该句起音早于 deadline − 0.5s ⇒ 回溯 gap 必 < merge_gap ⇒ b 也不触发。真正兜底的是单测(直接喂合成句) |
| c. `force_settle`(678) | `flush_paragraph=true` 且 `!speaking`(IME 分字符键 `'` = "我说完了") | 主动,50ms 内 | 句进行中 → 挂起重试;无段落 → 消费掉标记 |

`emit_paragraph_edge`:拼接 PCM(Arc,此后**逐句 clip 被逐出**,Arc 是唯一存活副本)→
`ParagraphEdge{paragraph: VadParagraph{batch_text: None, pcm, ..}, sr}` → 单句段落免重跑
(多句才段重跑)。

### 2.4 ActiveSession 生命周期

连续喂帧(不分段);**EOS / 段落 settle / 停滞 8s** 时重置——每个会话恰好覆盖
[上一边界, 本 EOS] ≈ 一句话(含边界静音,解码为空)。partial 节流 15 帧;partial 未变
超 8s = VAD 没锁住的微弱音频 → 重置防幻觉残留进下一句。

---

## 3. Pipeline 编排(round12:主循环 + 句任务 + 段任务)

### 3.1 主循环(`pipeline.run` future,select! 两源,单点 on_turn)

```
s1_rx.recv() ──┬─ StreamFragment → on_turn 直发(高频低延迟路径)
               ├─ Batch{pid, sentences, sr}:
               │    ① 只为 just-closed 句(sentences.last())spawn 句任务       ★ round13
               │    ② spawn live 整流任务(链式:prev = 本段上一个 live 任务)   ★ round13
               ├─ ParagraphEdge{paragraph, sr} →
               │      on_turn(ParagraphClosed{pid})     ← 先于段任务任何产出
               │      spawn(paragraph_task(paragraph, waits{句任务+live链尾}, ..))
               └─ SentenceBatchReady / ParagraphBatchReady → no-op(batch_jobs=false 不再产生)
turn_rx.recv() ── t → on_turn(t)(句/段任务产出,drain 单点 emit)
```

**边界时序的结构保证**:`ParagraphClosed(N)` 与段 N+1 的第一个 `StreamFragment` 同源
(s1_rx FIFO)、按 VAD 顺序产出 → wire 上**严格有序**。段 N+1 的 turn 类事件
(BatchSentence 等)只能由 Batch(N+1) 触发,而 Batch(N+1) 在 s1_rx 中必在
ParagraphEdge(N) 之后 → 亦晚于 ParagraphClosed(N) 的 emit。✓

`select!` 无 `biased`:两源同时就绪时**随机**选分支——只影响"修订类事件之间"的交错
(客户端按 id 归位,无语义破坏)。

### 3.2 句任务(`sentence_task`,pipeline.rs:433)

`spawn_blocking`:store 取句 clip → `recognize_once`(失败/空 = 合法 None,不重试——
实时优先)→ **先** `turn_tx.send(BatchSentence)`(仅 Some)→ 返回 `SentenceOutcome`。
remote ASR ~3.5s,完成序任意。

### 3.3 段任务(`paragraph_task`,pipeline.rs:463)—— 就绪门 = join!

```
join 句任务(逐个 await,完成一个处理一个):
    回填 acc[sid].batch → calibrate_paragraph(全句 best_text)
    → SentenceCalibration{pid}(live 联合整流,严格在该句 BatchSentence 之后)
段级重跑(仅多句段):recognize_once(concat PCM) → BatchParagraph{pid}
定稿整流:finalize_paragraph(全句 best_text,一次 LLM)
归档:record_final(PCM→archive,三份文本→day log + ring)
→ ParagraphCalibration{pid}
```

Stage2 无状态(校准器 `Arc<Mutex>` 串行,LLM 单飞);`PassThrough`(LLM disable)= 恒等,
`calibrated` 承载原文。

---

## 4. 事件目录(触发 × 发射点 × wire × 保证)

### Stage1Event(内部,消费循环 → 主循环,s1_tx FIFO)

| 事件 | 触发 | 载荷要点 |
|---|---|---|
| `StreamFragment` | partial 变化(≈0.3s 节流)+ **句定稿一条**(EOS) | `paragraph_id` = **起音开的真段键**(round13);句定稿条同键 |
| `Batch` | 每句 EOS | 该段**全部句**快照;新句 `batch_text: None`(in-flight) |
| `ParagraphEdge` | 关段(a/b/c 三路径) | `VadParagraph{batch_text: None, pcm: Arc}`;clip 随即逐出 |
| `SentenceBatchReady` / `ParagraphBatchReady` | 旧编排 batch worker | round12 `batch_jobs=false` 下**不再产生**(主循环 no-op 防御) |

### TurnEvent(主循环单点 on_turn)→ AsrEvent(wire,tag FROZEN)

| TurnEvent | AsrEvent(tag) | 触发 | 发射点 | 顺序保证 |
|---|---|---|---|---|
| StreamFragment | `stream_fragment` | partial 变化 / 句定稿 | 主循环直发 | 段内 FIFO |
| ParagraphClosed | `paragraph_closed` | ParagraphEdge | 主循环直发 | **先于下一段任何事件**(§3.1) |
| BatchSentence | `batch_segment` | 句任务完成 | turn_rx drain | 晚于该句 Batch;段内任意序 |
| BatchParagraph | `batch_window` | 段重跑完成(仅多句段) | turn_rx drain | 晚于 ParagraphClosed |
| SentenceCalibration | `segment_calibration` | **该句 BatchSentence 之后**(round17b:BS 到达触发 live 整流链) | turn_rx drain | 严格在该句 BS 后;段内链式串行(SC 顺序 = 段落生长序);全部先于 PCal;输入含已到 batch |
| ParagraphCalibration | `window_calibration` | 定稿整流完成 | turn_rx drain | 该段最后一条事件 |

wire 字段:段落 → `window_id`,句 → `segment_id`(旧词汇表 FROZEN)。

### 客户端折叠(Transcript,aura-agent)—— 前端假设的契约

- **段落 id 即顺序**:`BTreeMap<paragraph_id>`,`finals()` 按 id **降序**(最新在前),
  `active_paragraph()` = **id 最大的未关闭段**(transcript.rs:119/295/199);
- 首选预览只跟活动段(绝不跨段堆叠);段落关闭(`ParagraphClosed`/`BatchParagraph`/
  `ParagraphCalibration`)即进 finals 占位,后续按 id **REPLACED** 替换;
- `StreamFragment` 折叠进段句槽(**`!closed` 才写**,ghost 永远可写 → §7-B);
- 降级链:live 只由流式写;batch 缺席逐级回退流式拼接;校准只增强 calc,不污染 plain。

---

## 5. 端到端时间线(三个场景)

> **注**:以下时间线如实记录**修复前(round12)的行为**——幽灵段/假键的展示是
> §7-B 的病理解剖。round13 后:每段首句 partial 即携带真键(无 F 幽灵行),
> Batch 后立即有 SC(live 整流),其余交错关系不变。

### 5.1 同段两句(gap < merge_gap)—— 段内唯一全对的路径

**注意**:连第一句的 partial 都是预测键(说话期间 open=None,§2.3)。完整序列:

```
t0   说话1开始(open=None)
t1~  SF(F1,s1,partial)×N           F1 = last_win_id+1(假)
t2   EOS1:on_eos → 开段 p1(随机)→ SF(p1,s1,final) → Batch(p1,[s1])
     主循环:Batch → 句任务(p1,s1)
t2'  句任务完成 → BS(p1,s1) → SC(p1)(段任务未开,live 整流只在段任务里 → 无)
     ★ 注意:round12 下 SentenceCalibration 只由段任务发 → 段落关闭前**没有 live 校准**
t3   说话2开始(open=Some(p1))
t4~  SF(p1,s2,partial)×N           ← 键正确
t5   EOS2:gap<merge_gap → 入 p1 → SF(p1,s2,final) → Batch(p1,[s1,s2])
     主循环:再 spawn (p1,s1)+(p1,s2)                ← §7-D:s1 重投!
t6   静默 ≥ merge_gap(check_settle,deadline 唤醒)→ ParagraphEdge(p1)
     主循环:PC(p1) → 段任务(join×3[含 s1 重复] → 重跑(2 句)→ 定稿 → 归档)
t6'~ SC(p1)×3 → BP(p1) → PCal(p1)
```

客户端:`(F1,s1)` 幽灵段残留(§7-B);`(p1,*)` 正常折叠;finals(p1) 正确。
**候选 = 幽灵(active 若 F1 > p1)+ finals(p1)** → 看到重复/陈旧首选,50% 概率。

### 5.2 大停顿换段(gap ≥ merge_gap)—— 幽灵段诞生地(常见路径)

```
t0   说话1(段落 k)partial 键:首句假键 Fk,后句真键 pk
t1   EOS1 → SF(pk,s1,final) → Batch(pk,[s1]) → 句任务
t2   静默到 merge_gap → check_settle(未被抑制)→ ParagraphEdge(pk)
     → PC(pk) → 段任务(pk)
t3   说话2(段落 k+1)开始,open=None → partial 键 F(k+1)(假)
t4   段任务产出陆续到:BS(pk,s1) / SC(pk) / BP(pk) / PCal(pk)
     —— 与段 k+1 的 SF(F(k+1),..) 在 wire 上任意交错(协议允许,按 id 归位)
t5   EOS(句1) → 开段 p(k+1)(随机)→ SF(final) → Batch → …
```

客户端最终态:
- `pk`:关闭、定稿 → finals ✓
- `Fk / F(k+1)`:**永不开闭的幽灵段**,各含一句过时 partial,永远参与 active 竞争,
  永不进 finals,永不清理;
- 若 `F > p`(每段 ~50%):首选 = 幽灵的陈旧文本(用户说话时预览**冻结/倒退**)。

**实测"重复混乱"的形态来源**:finals(pk)(定稿完整文本)与幽灵/新段的 partial 残留
**并存于候选**(IME 侧把 finals + 预览组合成候选行)→ 同一句话出现两份、其中一份是
中途 partial(如"喂，喂，喂。现在出现了跟我严重的问题了啊" + "喂喂现在出现了更严重的
问题了啊!")。哪个幽灵胜出取决于随机 id 的大小比较——**概率性、每段必现**。

### 5.3 flush(IME "我说完了")

`flush_paragraph=true` 且 `!speaking` → `force_settle` → ParagraphEdge → 同 5.2 的段任务
链。说话中 → 挂起等 EOS(50ms tick 重试);无段落 → 消费标记。

---

## 6. 时序保证的真实边界(server 实际保证了什么)

| 保证 | 成立 | 依据 |
|---|---|---|
| 段内 SF 顺序 | ✓ | s1_tx FIFO |
| `ParagraphClosed(N)` 先于段 N+1 任何事件 | ✓ | 同源 FIFO + 任务触发依赖(§3.1) |
| `SentenceCalibration` 严格在该句 `BatchSentence` 后(round17b) | ✓ | BS 到达触发链式 live 任务(架构:batch 完成 → 之后纠偏);迟到的 BS(段已关)不再触发,PCal 已回填 |
| 该段全部 SC 先于 `ParagraphCalibration` | ✓ | 段任务先 await live 链尾再定稿 |
| `ParagraphCalibration` 是该段最后事件 | ✓ | 段任务末尾 |
| 跨段修订类事件不破坏已关段 | ✓ | 协议按 id REPLACED;round13 后无幽灵段,PC 后不再有该段 SF |
| paragraph_id 单调(客户端排序依据) | ✓(round13) | 时间戳 id,严格递增 |
| live partial 的段键 = 实际归属段 | ✓(round13) | 起音即开段,真键前置 |

---

## 7. 前后端时序错位清单(现状不可用的根因)

### A. 段落 id 随机 vs 客户端"id 即顺序"——契约断裂(最根本)【round13 已修】

- **server**:`next_random_win_id()`(recognizer.rs:589)刻意随机("避免可预测性");
- **client**:transcript.rs:119 注释明言"`paragraph_id`(daemon 端**单调递增**)",
  `BTreeMap` + `finals()` 降序 + `active_paragraph()` = max 未关闭 id,全部以 id 为序;
- **lib.rs:48** 的 `ParagraphId` 文档也声称 "monotonic within a run"——三方各说各话;
- **症状**:finals 候选顺序随机颠倒(实测"候选 2,3,4 顺序反、最新句子在候选 4");
  active 段选择错误(旧段 id 大者胜出);`sync_history` 后顺序同样乱;
- **修复**:id = 创建时刻时间戳微秒(`next_win_id`,严格递增)。

### B. prospective 假段键 → 幽灵段(高频、每段必中)【round13 已修】

- **server(修复前)**:live partial 的段键来自 `prospective()`;`open=None` 时 =
  `last_win_id+1`(纯预测),与 EOS 时 `next_random_win_id()` 分配的真键**永不相等**。
  触发面 = **每个段落的首句**(段落总是在静默期关闭,首句说话期间 open=None);
- **client**:幽灵段永远收不到 `ParagraphClosed`(真键才有关闭事件)→ 永远"未关闭" →
  ① 永远参与 `active_paragraph()` 竞争(id 比大小,~50% 胜出 → 首选冻结在首句陈旧
  partial);② 永不进 finals(说过的话"消失"感);③ `BTreeMap` 无界泄漏;
- **变体(2.3-b 路径,几乎不可达)**:下一句 partial 以 `(pk, s_next)` 折叠进真段 pk、
  PC(pk) 后残留在 finals——几何上被 lead-in 抵消(见 §2.3 表注),仅作防御保留;
- **根因**:`ParagraphClosed(N) 后不再有段 N 的 SF` 这条 server 语义,被"PC 之后仍到达、
  但键是假段"的 SF 破坏——客户端无法区分"新段开流"与"幽灵残留";
- **修复**:**起音即开段**(`on_speech_onset`,rising edge 分配真键)+ 空段 GC
  (从未出句的开段,静默满 merge_gap 静默丢弃)。

### C. ParagraphClosed 时点(已核实:非问题)

- 实际关段走 check_settle(静默满 merge_gap 即关,deadline 唤醒,及时)与 flush;
  `settle_if_gap` 因回溯 onset 含 lead-in 几何上几乎不可达(§2.3 表注);
- 边界信号本身及时且有序(§6 前两行保证成立)——**错位不在边界时点,而在 A/B 的键与序**。

### D. round12 Batch 全量重投 —— 成本二次放大回归(功能正确但不可上线)【round13 已修】

- pipeline.rs:337(修复前):`for s in sentences` 对 Batch 载荷(**该段全部句**)逐句
  spawn 句任务 → 每句 EOS 都为**全部历史句**再投一次:N 句段 = N(N+1)/2 次 ASR + 同数
  次 live LLM 整流 + 段任务 join 膨胀;客户端幂等(按 sid upsert)所以**不破显示**,但
  remote ~3.5s/次下延迟与费用爆炸;
- 旧编排(finalize_sentence 只投 just-closed 句)无此问题;Batch 载荷"全量快照"是为
  **无状态 Stage2** 设计的,不适合当任务触发器用;
- **修复**:主循环只为 `sentences.last()`(just-closed 句)投句任务。

### E. 次要(记录在案)

1. `select!` 无 biased:修订类事件交错随机(协议内,无害);
2. 幽灵段无清理(与 B 同源)——**随 B 修复消失**;
3. ~~round12 下 `SentenceCalibration` 只由段任务发 → 段落关闭前没有 live 校准~~
   **round13 已修**:live 整流回归 Batch 臂(`live_calibration_task`,段内链式串行),
   SC 顺序 = 段落生长序,且全部 SC 先于 PCal(段任务等 live 链尾后定稿)。注意语义:
   SC 在 **Batch 事件时**触发(新句用流式文本,batch 未回回退流式),与其
   `BatchSentence` 无先后约束(§4 表已更新);
4. 单句段落/段重跑 `batch_asr_ms = 0`(计时丢失,性能埋点失真);
5. `Batch` 载荷内句 `batch_text` 恒 None(协议如此)——`best_text` 在段关闭前只有流式可回退,
   live 整流输入质量受限(设计取舍,非遗漏)。

---

## 8. 修复方向(round13 拍板与落地)

1. **id 契约统一(A)** ~~段落 id 改回单调递增……~~ → **已落地:id = 创建时刻时间戳**
   (微秒,严格递增;兼具可读性 —— id 即段落创建时刻,日志可直接读出)。
2. **幽灵段根治(B)** ~~三选一~~ → **已落地:方案 a(server 真键前置)**——detected()
   rising edge 即开段分配真 id(`on_speech_onset`),SOS 降级为防御兜底;配套空段 GC。
3. **Batch 重投(D)** → **已落地**:主循环只为 `sentences.last()`(just-closed 句)投任务。
4. **live 校准回归(E.3)** → **已落地**:Batch 臂触发 `live_calibration_task`(段内
   链式串行,SC 顺序 = 段落生长序);段任务等 live 链尾后定稿(全部 SC 先于 PCal)。
