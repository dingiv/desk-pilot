# aura 流水线 —— 双阶段识别:架构与实现(as-built 2026-09-01 · round27)

> **一句话**:声音进来,流式先出粗稿(实时),batch 再出精稿(秒级),Stage2 拿**两路结果对照**做终稿。
> 边界信号("我说完了")及时,精度("说得对不对")后到后改——**渐进精化**是这套架构的核心形态。
>
> 代码为准(合并自原 pipeline/stages/new-pipeline 三文,2026-09-01)。
> 排障见 [debugging.md](debugging.md);crate 全貌见 [architecture.md](architecture.md)。

---

## 1. 两个 Stage,两个间隔

|                     | 角色                                                       | 实现                                            |
| ------------------- | ---------------------------------------------------------- | ----------------------------------------------- |
| **Stage1 语音识别** | 听:VAD 门控 + 流式 ASR(实时粗稿)+ batch ASR(精稿)          | Silero VAD + zipformer 流式 + 远/近端 batch ASR |
| **Stage2 语言纠偏** | 改:LLM 对**两路识别结果**做整流(标点/同音字/专名/句首补全) | OpenAI 兼容 LLM;`PassThrough` 可禁用(恒等)      |

两个静音间隔切出**两级实体**——这是整条流水线的骨架:

| 间隔         | 配置                                   | 切出        | 语义                                       |
| ------------ | -------------------------------------- | ----------- | ------------------------------------------ |
| **句子间隔** | `min_silence`(1s)                      | `Sentence`  | 原子录音片段:独立流式会话 + 一次句级 batch |
| **段落间隔** | `merge_gap`(当前部署 3.5s,代码默认 5s) | `Paragraph` | 定稿单位:多句组合,拼接 PCM 整段重跑        |

有效段落间隔 = 句间静音落在 (1s, 3.5s) 区间:短停顿切句不切段,大停顿才关段。
段落 id = **创建时刻时间戳**(单调递增,id 即说话顺序)。

---

## 2. 全景:线程模型与数据流(round27 文件 = 线程模型)

```
omni-scout /audio (TCP)
   │  ① 拉流+检测线程(blocking 池常驻;front.rs ingest_loop;自动重连;
   │    VAD 逐帧检测同在此线程):scout chunk → 重切 32ms 窗 → Stage0VAD.feed
   ├─ 门控帧(detected + lead_in Onset)──stream 通道──▶ 流式任务(帧唯一去向)
   └─ FrontEvent{detected, events, onset} ──有界队列(10min 环回)+ Notify──▶ 大脑
        (断流>2s 且有 partial → 前端喂静音逼 EOS)
   ▼
┌──────────────────── Stage1(音频 → 文本,边界范式)────────────────────┐
│ 大脑 = consume_loop(loops.rs;纯决策,检测/门控已下沉):              │
│ 起音即开段(onset 随 FrontEvent,时间戳真 id)+ 分句/段落边界(tracker)   │
│ 流式任务(stream.rs,独立 tokio::task;一对通道):                      │
│   → ~0.3s 节流出 StreamFragment(只进 UI,不是 Stage2 输入)             │
│ VAD 间隔(min_silence 1s)→ EOS:定稿交接(回执同通道)→ 句 PCM           │
│   → Batch { paragraph_id, sentences: 段内全部句 }                       │
│ merge_gap(部署 3.5s)→ 段关闭:settle / 下一 SOS 间隔 ≥ 它              │
│   → store.concat 拼接 PCM → ParagraphEdge(整段重跑由段任务自建)        │
└────────────────────────────────────────────────────────────────────┘
   │  tokio 通道 → main_loop select!(loops.rs;唯一发射点,统一留痕)
   ▼
┌──────────────────── 编排任务(batch.rs)──────────────────────────────┐
│ Batch → 句任务(异步轨 recognize_once_async)→ BatchSentence           │
│ BS 到达 → live 整流任务(段内链式串行,输入=双通道)→ SentenceCalibration │
│ ParagraphEdge → 段任务(join 句任务 + live 链尾 → 段重跑(多句)→        │
│   BatchParagraph → 定稿整流一次 → 归档)→ ParagraphCalibration          │
└────────────────────────────────────────────────────────────────────┘
   ▼
daemon on_turn 回调 → SSE 数据面 /api/asr_stream → 前端级联折叠(见 §9)
   + record_final 落盘(recordings WAV + turns jsonl,按 paragraph_id)
```

### 2.1 文件地图(`crates/aura-core/src/pipeline/`,依赖单向无环)

```
types ← spec/tracker/calibrator/vad ← stream/front ← resources ← loops/batch ← mod
```

| 文件 | 行数≈ | 职责 |
|---|---|---|
| `mod.rs` | 235 | 组装车间:Pipeline + assemble + stage1_config/stage2_calibrator(薄) |
| `spec.rs` | 124 | 选型纯数据(PipelineSpec/VadSpec/StreamSpec/AsrSpec/LlmSpec) |
| `types.rs` | 166 | 跨模块纯类型叶子(FrontEvent/StreamCmd/…/SettledParagraph/TurnEvent) |
| `loops.rs` | 1080 | **两个循环**:main_loop(select! 主循环,唯一发射点)+ consume_loop(大脑) |
| `resources.rs` | 430 | 资源+配置+生命周期:mgr/vad/front_q/store、构造、run_ingest、recognize_once |
| `front.rs` | 130 | Stage0 拉流线程:ingest_loop(门控/lead_in/断流静音)+ speech_pending |
| `vad.rs` | 70 | 检测引擎缝:Stage0VAD trait + SileroVAD(**换 VAD 只动这文件**) |
| `stream.rs` | 200 | 流式任务(accept/decode,0.29s 节流,Finalize 回执) |
| `tracker.rs` | 500 | ParagraphTracker 纯边界数学(可单测;测试在本文件) |
| `batch.rs` | 270 | 任务壳:sentence / live_calibration / paragraph + emit_turn 留痕 |
| `calibrator.rs` | 400 | Stage2CalibratorImpl + trait |

### 2.2 执行载体

**常驻阻塞线程只有拉流+检测**(scout TCP → 重切 → Stage0VAD.feed → 门控帧直发流式
+ FrontEvent 入队;断流喂静音也在此);其余都是 tokio 任务协作分时——大脑(分句/段落
决策)、流式识别任务(与大脑仅一对通道)、每句 batch 任务与纠偏任务、每段重跑与定稿
任务。batch/LLM 走**异步路由**(`dp_models::AsyncAsr`/`AsyncLlm`:远程 Http 原生
await、超时可被 `tokio::time::timeout` 真取消;本地 ONNX/本地 LLM 在路由内部
`spawn_blocking` 干 CPU 真活;段落落盘亦 spawn_blocking)。**main_loop select!
是唯一的发射点**,所有发往前端的事件先统一留痕再发。模块不 spawn 线程,全部由
`pipeline/` 编排创建;宿主:daemon = `rt.spawn(pipeline.run)`,零专用线程。

### 2.3 通道清单(谁喂谁、什么频率)

| 通道 | 方向 | 频率 | 载荷 |
|---|---|---|---|
| FrontEvent 队列(有界 10min 环回)+ `Notify` | 拉流线程 → 大脑 | 31Hz 恒定 | `{detected, events, onset}`(样本不随行) |
| `StreamCmd`(cmd) | 拉流线程(Onset/Feed,门控直发)+ 大脑(Finalize/Reset)→ 流式任务 | 高频(语音期 31Hz)+ 低频控制 | `Feed` 帧 / `Onset{at, lead_in}` / `Reset` / `Finalize` |
| `StreamOut`(out) | 流式任务 → 大脑 | ~0.3s partial + 每句一条 | `Partial` / `Finalized`(定稿回执,同通道) |
| `s1` 事件通道 | 大脑 → 主循环 | 低频 | SF / Batch(EOS)/ ParagraphEdge(settle) |
| `turn` 事件通道 | 各任务 → 主循环 | 低频 | BS / SC / BP / PCal |

高频(帧)与低频(事件)在通道上分层,互不阻塞;batch 从头到尾不见实时帧——它吃的是
EOS 定稿交接的整句 PCM(恰为流式任务累积的那份,含 soft onset)。

---

## 3. 工作原理(沿时间轴走一遍)

### 3.1 起音 —— 流水线启动

Stage1 由 VAD 快速控制启动:**检测到声音的瞬间**(detected() 翻转),创建一个新的
paragraph(分配时间戳真 id)和一个新的 sentence(流式 partial 从第一条起就携带真实键),
同时补喂起音前 ~0.5s 的 lead-in——软起音不丢,流式和 batch 听到同一句话的完整音频。

### 3.2 句内 —— 流式粗稿

说话期间,流式 ASR 每 ~0.3s 解码一次 partial,文本逐条推给前端(首选候选持续生长,
这就是听写的"实时感")。热词偏置在这一路生效。

### 3.3 句间隔(1s 空白)—— 句级 batch + 纠偏

VAD 识别到 1s 空白,**关闭当前句子,创建新句子**,触发一次 **Batch 识别动作**(该句完整
PCM 送 batch ASR);batch **完成之后**,Stage2 触发一次**纠偏动作**——二者有明确先后:
**先有识别结果,才谈纠偏**。纠偏的输入是**双通道**:batch 识别结果 + 流式识别结果同时
传给 Stage2(`input: both` 是默认;batch 权威、流式补句首/热词,两路对照)。

之后的流式识别结果进入新的第二个句子中去——与上一句的 batch/纠偏**并行**,互不等待。

### 3.4 段间隔(3.5s 空白)—— 整段 batch + 整段纠偏

VAD 识别到 3.5s 空白,关闭段落,触发一次 **Paragraph 整段 batch 识别动作**:把本段所有
语音片段**拼接成完整音频**,再次进行 batch 识别(跨句上下文重听,多句段才跑;单句段复用
句级结果)。之后 Stage2 触发一次**整段纠偏动作**:整段的流式识别结果拼接起来,连同整段
batch 识别的结果,一同交给 Stage2——这是该段的**定稿**。段落关闭的边界信号先于任何
后续事件到达前端,前端立即知道"这段说完了",定稿稍后修订到位。

### 3.5 事件流(一句话的一生)

```
起音 ──► stream×N(流式 partial,~0.3s/条)
1s 空白 ─► stream(final,句定稿流式)
        └► [句级 batch 识别,异步,~百ms/远端秒级]
              └► batch_sentence(batch 文本)          ← 先
                    └► sentence_calibration(纠偏)     ← 后:双通道输入,严格在 batch 之后
3.5s 空白 ─► paragraph_closed(边界:文本定格)
           └► [整段拼接重跑,多句段]
                └► batch_paragraph(整段权威文本)
                      └► paragraph_calibration(定稿纠偏)= 该段最后一条事件
```

---

## 4. batch 高延迟与流式的交错

### Q1:接下来的第二句的流式识别事件怎么办?会被第一句的 batch 阻塞吗?

**不会。** batch 识别在独立的异步任务里执行;句关闭时大脑只发事件(微秒级)就继续。
第二句从自己的起音就开始流动,与第一句的 batch/纠偏完全并行。

### Q2:第 1 句话的结束,是等 batch 完成,还是 VAD 检测到 1s 空白之后就立即开始?

**立即开始。** 1s 空白瞬间,句子在逻辑上已定稿:流式文本即刻权威(final 事件先发),
batch 只是几秒后到达的**修订**。句子定稿是两级的:先流式(立即可用),后 batch(更准)。

### Q3:第二句流式已在产生、第一句的 batch 和 calibrate 还没完成,前端会怎么样?

前端看到的是**交错流 + 文本渐进精化**——这是协议设计的出发点,不是故障:

```
t0        stream "第一句…"(粗稿,首选候选)
t0+0.1s~  stream "第二句partial…"      ← 与下面任意交错
t0+Δ      batch_sentence "第一句。(精稿)" ← 第一句文本被替换,变准
t0+Δ'     sentence_calibration "第一句。" ← 纠偏(双通道)再替换
3.5s 后    paragraph_closed → … → paragraph_calibration(定稿,进历史)
```

- 前端**绝不串行等待**:事件都带 `(paragraph_id, sentence_id)`,迟到的是**定位修订**
  (REPLACED 语义),不是时序依赖;
- 每句话在前端经历`流式粗稿 → batch 精稿 → 纠偏终稿`三级演进,每次变准;
- 段落关闭即以现有最佳文本占位进历史("说过的话永远可见可提交"),后续修订按 id 替换。

---

## 5. Stage1 实现细节(ONNX 语音前端)

**两级实体**(`types.rs`/`lib.rs` 数据契约区):

| 实体 | 边界 | 内容 |
|---|---|---|
| `VadSentence` | VAD 间隔(min_silence) | id、audio_id、start_s/end_s、streaming_text(句级流式定稿)、batch_text: **Option**(远程失败合法) |
| `VadParagraph` | merge 段(merge_gap) | 句快照、拼接 streaming、段级 batch(重跑,权威)、pcm: Arc(settle 拼一次,store 随即 evict) |

**流式引擎**:恒本地——`zipformer`(默认)或 `x-asr`(`asr.stream.model`;自带标点;
tokens.txt 必须保持官方"token id"两列格式)。**batch 后端**:`Arc<dyn AsrProvider>`
三选一(local SenseVoice / remote HttpAsr(3s 硬超时+断链熔断)/ Disabled 恒空回退)。

流程细节:

1. **采集+检测(拉流线程)**:ingest_loop scout TCP(客户端可 `?chunk_ms=N` 请求聚合
   推流)→ 重切 32ms 窗 → `Stage0VAD::feed`(SileroVAD 默认,trait 为换引擎缝)→
   门控帧直发流式、FrontEvent 入队唤醒大脑;断流 >2s 且当前句有 partial → 前端喂
   合成静音逼 EOS(读超时 100ms 钩子)。时钟:拉流线程与大脑共用同一 `start: Instant`
   原点(量尺统一,round26 教训)。
2. **流式任务 = 独立 tokio::task**:指令通道收 `Onset`/`Feed`(拉流线程直发)/
   `Reset`/`Finalize`(大脑发),accept/decode(ONNX 前向)全在流式任务里——与 VAD/
   分句/段落定稿零共享执行流;partial 回传后仍由大脑发射(两任务汇于同一事件出口,全序不破)。
   会话是**持续喂帧 + 边界重置**(D1 落地:sherpa 的 SOS 与 EOS 成对回溯,不存在"起点
   建会话"时机),恰好覆盖 [上一边界, 本次 EOS] ≈ 单句。partial 每 9 窗(≈0.3s)解码、
   变化才发 `StreamFragment`;EOS 定稿回执走同一通道(`Finalized`)。
   **停滞看门狗**:partial 非空但 ≥8s 无变化且无 EOS ⇒ VAD 从未锁定 ⇒ 重置会话
   (微弱音频残留/流式幻觉不得卷入下一句)。**说话中无实时纠偏**(S-D2)。
3. **句定稿**(EOS):流式任务 finalize 交接(PCM + 定稿文本,几十 ms)→ PCM 入 store
   (共享 `Arc`)→ `Batch { paragraph_id, sentences }`(载荷即整段,`batch_text: None`
   为 in-flight;句级 batch 由 batch.rs 句任务自建,`recognize_once_async`(AsyncAsr
   路由:远程 HttpAsr 原生异步,本地 ONNX spawn_blocking 桥),结果以 `BatchSentence`
   回传)。噪声句不在 EOS 丢弃(异步后 EOS 时刻只有流式文本,
   丢弃会吞"流式空 batch 有"的真实语音)。
4. **段定稿**:`ParagraphTracker` 判边界——下一句起音间隔 ≥ merge_gap(用起音墙钟,
   round26),或静默超时(`check_settle`,句进行中/speaking 抑制);起音即开段
   (rising edge 分配时间戳 id,partial 从第一条起携带真键);空段静默满 merge_gap 即 GC。
   `emit_paragraph_edge` 拼 PCM(段落持 `Arc`)→ `ParagraphEdge` → evict。
   **单句段免重跑**:只有一句时拼接 PCM 与该句完全相同,复用句级结果。段任务 join
   全部句任务(就绪门)+ live 链尾 → 段重跑(多句;异步 + `tokio::spawn` panic 隔离
   + 15s 兜底**真取消**,**PCal 必发**)→ 定稿整流一次(异步路由)。
5. **AudioStore**:`Mutex<BTreeMap<id, PCM>>`,容量按样本(10min ≈19MB),超限逐最旧。
6. **VAD 门控流式**:`detected()` 实时信号是流式喂帧的唯一门卫(空闲零喂帧零解码);
   起音翻转时补喂最近 ~0.5s lead-in(soft onset 靠它进会话);`accept_waveform` 与
   `pcm` 喂完全相同的帧 → 流式与 batch 听到同一句音频(共享 PCM 不变式)。

---

## 6. Stage2 实现细节(LLM 联合整流)

**位置**:`calibrator.rs`(`Stage2CalibratorImpl`)+ `prompt.rs`;任务壳在 `batch.rs`
(异步路由:远程 HttpLlm 原生 await、本地 spawn_blocking 桥,LLM 耗时不卡 partial)。

**无状态**:每次调用都是纯函数式——输入是"整段全部句的文本"(payload 即段落)。

- `calibrate_paragraph(paragraph_id, sentences)`:全部句 `best_text()` 逐行联合整流 →
  `SentenceCalibration`。**触发点 = 每句 BS 到达**(架构要求"batch 完成 → 之后纠偏,
  先后明确"),段内链式串行(SC 顺序 = 段落生长序);输入带 `segment_id` = 覆盖上界
  (前端零派生状态即知覆盖谁)。
- `finalize_paragraph(paragraph)`:用全句最终 `best_text()`(句任务 join 回填;缺失句
  回退流式)**跑一次 LLM** → `ParagraphCalibration`(段粒度定稿,S-D3)。全空段零 LLM。
- 纠偏输入源 `llm.input`:**both 为默认**(双通道信封,LLM 对照补回批式丢的句首);
  batch/stream 为显式降级。
- LLM 失败回退原文;用户纠正对(环形 20 条,POST /api/correct)优先级最高注入;
  热词 store 每次读最新(prompt 热词块停用——小模型遵循不佳)。

---

## 7. 事件协议(wire)与不变式

**SSE 事件类型**(`AsrEvent`,aura-agent/view.rs;**字段名冻结旧词汇**——Rust 侧
sentence/paragraph 改名经 serde rename 回 `window_id`/`segment_id`,预构建 Web SPA 与
存量日志不受影响):

| 事件(SSE type) | 键 | 触发 | 性质 |
|---|---|---|---|
| `stream_fragment` | window_id + segment_id | 流式 partial / 句定稿 | 实时流 |
| `batch_sentence` | window_id + segment_id | 句级 batch 完成 | 修订 |
| `sentence_calibration` | window_id + **segment_id(覆盖上界)** | **该句 batch 完成之后** | 修订(双通道输入) |
| `paragraph_closed` | window_id | 3.5s 空白 / 主动归档(flush) | **边界**(先于下一段任何事件) |
| `batch_paragraph` | window_id | 整段重跑完成(多句段;单句段不发) | 修订 |
| `paragraph_calibration` | window_id | 定稿纠偏完成 | 该段最后一条 |
| `correction` | window_id | 用户纠正标记 | 反馈通道 |

**关键不变式**:

1. `paragraph_closed(N)` 严格先于段 N+1 的任何事件(边界时序,server 结构保证);
2. `sentence_calibration` 严格在该句 `batch_sentence` 之后(先有结果,再纠偏),
   且**携带触发句 id** = 覆盖上界;
3. `paragraph_calibration` 是该段最后一条事件(重跑/纠偏挂死也有兜底超时,**必发**);
4. 段落 id = 时间戳,单调递增——id 即顺序,客户端按 id 归位修订,不依赖到达顺序。

**实现不变式**(重构期钉死):帧只进流式,batch 只吃 EOS 定稿的整句 PCM;partial 只进
UI 不是 Stage2 输入;常量:PARTIAL_EVERY_FRAMES=9(~0.3s)、STALE_SESSION_RESET=8s、
VOICE_SETTLE_MARGIN=0.6、min_silence=1s、merge_gap=3.5(部署)、PARAGRAPH_RERUN_TIMEOUT=15s;
idle 深睡/复醒(running + resume Notify)与 flush_paragraph 主动归档语义不变。

识别事件走数据面(直推不节流);设置变更走控制面(version ping → 重拉快照)。
存储按段:`record_final` → recordings WAV + turns jsonl。Stage3 规则触发器不变。

---

## 8. 降级链(优雅退化)与能力边界

| 场景                                | 行为                                                      |
| ----------------------------------- | --------------------------------------------------------- |
| batch 识别失败/超时/空              | 该句无 `batch_sentence`,回退流式文本(流式是底线,本地可靠) |
| Stage2 禁用(`llm.backend: disable`) | 纠偏 = 恒等,事件形状不变,calibrated 承载原文              |
| 整段重跑挂死/崩溃                   | 兜底超时,回退句级 batch 拼接,定稿照发                     |
| 流式整句无输出(模型失手)            | batch 兜住,该句文本在 batch 完成时一次到位                |
| 断连重连                            | 历史定稿经 `/api/results` 全量补齐,新流按 id 无缝续接     |

**文本优先级**(任何一层取 best):整段 batch > 逐句拼接(句 batch > 句流式);纠偏输入
= 双通道对照。校准只增强显示,永不污染底线文本。

能力边界:流式是小模型,粗稿与精稿的差值就是可见的"文本修正跳变"(范式固有代价);
切句/切段完全由静音间隔决定;近讲单说话人假设(无远场/多说话人/回声消除);显示延迟
下界 = partial 0.3s 节流,句级精稿 = EOS + batch 耗时,段定稿 = +3.5s;同一时刻只有
一个开放段落(单段流)。

---

## 9. 前端处理:级联折叠

前端不做事件重放,而是把交错的事件流**折叠**成一个极小的展示状态:**一个 live 段落 +
最多 4 个历史段落**。事件按 `(paragraph_id, sentence_id)` 定位写入,读取时逐层择优——
乱序到达被折叠吸收,任何时刻读出的都是"当前最佳"。

### 9.1 展示窗口与排列

```
1. live    ← 永远是最新的那个段落(说话中,频繁刷新)
2. 第4段   ┐
3. 第3段   │ 历史段落(已关闭/已定稿),最新在前、最旧在后
4. 第2段   │ 按段落 id(时间戳)降序,最多 4 个
5. 第1段   ┘
```

- **live 必定存在**(说话期间必有开放段);历史段落一开始没有——第一句话的 live
  折叠出来之前,窗口里只有它自己;
- 段落关闭(边界信号到达)即从 live 让位、以现有最佳文本**占位**进入历史(说过的话
  永远可见可提交);后续迟到修订按 id **替换**(REPLACED),不换位置;
- live 永远只跟"最新"绑定:说下一段时干净切换,绝不与历史堆叠拼接。

### 9.2 live 的级联规则

live 段落内部维护**该段每个句子的状态机**(流式文本槽 + batch 文本槽):

```
live 显示 = 纠偏结果(若有) > batch 拼接(若有) > 流式拼接(兜底)
```

- **只有最新的那一个句子才会出现流式识别的结果**——老句早已被它们的 batch 顶替;
- 纠偏到达整段顶替;没有纠偏时逐句 batch > 句流式;live 文本随事件**渐进精化**,
  每次跳变都是变好,方向单调。

### 9.3 历史段落的级联

每个历史段:`定稿纠偏(PCal)> 整段 batch(BP)> 逐句拼接(句 batch > 句流式)`——
与 live 同一条优先级链,多一层"定稿"。迟到修订按 id 落回自己的槽位替换文本,
**永不改变排列顺序**(id 即顺序)。

### 9.4 与协议的对接

- 边界信号(`paragraph_closed`)是 live→历史的**换位时机**;已关闭段落的迟到流式
  事件**完全忽略**(不留幽灵段);断连重连:历史定稿经 `/api/results` 全量补齐;
- 实现:`aura-agent` 的 `Transcript`(折叠状态机)+ `ime-core` 的 VoiceMember
  (候选组装:`cascade_preview` + `finals`,窗口 = live + 4)。

---

## 附录 A:设计决策

**边界范式决策(2026-08-17 拍板,原 vad-segment-model.md)**:

- **S-D1 流式会话粒度 → 句级会话**:实施修正——sherpa VAD 的 SOS 与 EOS 成对回溯,
  落地为持续喂帧 + 每句 EOS / 段 settle 后重置会话,效果等价句级会话。
- **S-D2 说话中实时纠偏 → 砍掉**:说话中 UI 只显 raw partial,纠偏在首个 VAD 间隔后
  (batch 完成 → 之后纠偏)。
- **S-D3 段关闭产出 → 段级 Final**:一段定稿一条(段内多句联合整流的完整文本)。
- **S-D4 迁移策略 → 直接替换**:executor/calibrator/daemon/aura-agent/前端同步切换。
- **Stage2 窗口状态机 → 无状态化**(batch 异步后):载荷即段落,内部不存状态;
  就绪定稿由段任务 join 结构承担。
- **段 PCM**:settle 拼接一次为 `Arc<Vec<i16>>` 挂在 VadParagraph 上(段重跑与落盘
  共用),store 随即 evict。

**Stage0/Stage1 分界施工决策(2026-09-01,new-pipeline R1-R5)**:

- **N-D1** `Stage0VAD` trait:`feed/detected/last_voice_at`,`&self`+内部可变,
  `Arc` 双线程共享(前端写、大脑读);**N-D2** `SileroVAD` 默认实现,`speech_pending`
  留自由函数(VAD 不知道 partial——ASR 概念不进引擎);**N-D3** FrontEvent 单 FIFO
  取代 AudioRing(配对由队列顺序保证,满丢旧=环回);**N-D4** 帧由前端直发流式,
  大脑持 tx 克隆只发 Finalize/Reset;**N-D5** batch 保持每句短命 `spawn_blocking`
  (常驻服务会队头阻塞);**N-D6** batch 触发者是大脑(段落归属 + BS 排在句尾 SF 后);
  **N-D7** calibrator=join 语义不动;**N-D8** 前端发射永远单点;**N-D9** 时钟同源
  (拉流与大脑同一 `start: Instant`);**N-D10** 断流喂静音随 VAD 住前端(读超时钩子)。

---

## 附录 B:施工沿革(压缩版;完整 round 表见 debugging.md)

round13-20b 时序错位修复/全异步化/双通道输入/前端级联对齐/SC 覆盖上界走协议 →
round21-24 流式独立任务 + 模块化 + 通道收敛 → round25/26 日志分级 + settle 量尺统一
(修"同句中途换段")→ **R1-R5(2026-09-01)**:Stage0VAD trait、VAD 下沉拉流线程、
FrontEvent 队列、帧直发流式、断流静音进前端、模块重划分(types/spec/front/loops/
batch/resources,文件=线程模型,依赖单向无环)。全程:构建 0 error / 测试不減
(387→383=删 buffer 死测试 4 个;381 含用户 IME 会话未提交改动 −2)/ clippy 基线
4 条 / wire 零变化。
