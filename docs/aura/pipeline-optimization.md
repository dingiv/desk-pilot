# 语音识别管线优化设计（as-built 2026-08-30 · 以 async-batch 为基础）

> **状态：P0 已实现（2026-08-30，dev 分支），P1–P4 未实现。** 代码为准。
> P0 落地形态与 §3 摘要有一处偏差：batch 用**单个 worker 线程**（N=1 FIFO，用户指定
> “添加一个独立的线程”）而非 N 线程池 —— 连续说话时 job 可能积压、定稿延迟受排队影响；
> 升级成池只需在 `pipeline.rs` 对同一条 job 通道多 spawn 线程，事件契约不变。
> 另：P0 落地时加了一条**线程归属约束** —— Stage1/Stage2 只暴露阻塞函数，所有线程
> （ingest / batch / stage2）由 `pipeline.rs` 统一创建。
> 本文在 [`async-batch-design.md`](async-batch-design.md)（把 batch 移出消费线程）基础上，
> 扩展为**整条管线的优化设计**：先定位当前管线**全部**瓶颈（热路径阻塞、O(n²) 整流、
> 冗余重跑、首字延迟），再给出**分层**方案（P0–P4），每层独立可落地、可灰度、可回退。
>
> 生产配置（`apps/audio-aura/aura.yaml`）：`asr.backend: remote`（qwen3-asr，经 dp-router，
> 实测 ~3.5s/次）+ `llm.backend: remote`（qwen2.5-3b）+ `merge_gap: 3.5s` +
> `min_silence: 1.0s` + `threshold: 0.1`。以下所有成本估算以此为准。
> 代码入口：Stage1 = `crates/aura-core/src/recognizer.rs`，Stage2 = `crates/aura-core/src/calibrator.rs`，
> 组装 = `crates/aura-core/src/pipeline.rs`，前端折叠（消费方）= `crates/ime-core/src/voice_state.rs`。
> 契约权威：[`stages.md`](stages.md)。

---

## 1. 瓶颈地图（现状 as-built）

管线是四级流水线：`ingest → 消费循环（VAD+流式+batch+边界）→ Stage2 worker（LLM 整流）→ 前端折叠`。
逐个慢操作定位：

| 操作 | 位置（线程） | 耗时 | 问题 |
|---|---|---|---|
| Silero VAD | 消费循环（热路径） | ~ms/帧 | 本地，快 —— **OK** |
| 流式解码（zipformer） | 消费循环（热路径） | ~几十 ms/次（每 15 帧，~0.5s） | 本地，快 —— **OK**，但首字见 §P3 |
| **句级 batch** | **消费循环（热路径）** | **远程 ~3.5s/次** | **★ 阻塞音频 → 吞句 bug + 流式冻结**（`recognizer.rs:804`） |
| **段级 batch 重跑** | **消费循环（热路径）** | 远程 ~3.5s（整段音频） | 阻塞 + 与句级 batch **冗余**（重听整段）（`recognizer.rs:620`） |
| **LLM 整流** | `aura-stage2` worker | 远程 ~1–3s/次 | **★ O(n²)**：每句整流全段；**且单线程串行** → 定稿被积压延迟（`calibrator.rs:102` / `pipeline.rs:237`） |
| 首字延迟 | 消费循环 | ~1s | VAD 起音 + lead-in 0.5s + 首帧解码 0.5s |

**核心洞察**：管线有**两个慢 I/O（batch、LLM）+ 一个冗余操作（段级重跑）**。
- **batch 落在音频热路径**（最严重）：阻塞消费循环 → 吞句 bug + 流式 partial 冻结。
- **LLM 整流是 O(n²)**：每句 EOS 都整流"全段至今"，N 句 = N 次调用、总输入 ≈ N²/2 句单位；
  且在**单一 worker 上串行**，`ParagraphEdge`（定稿）排在全部 N 次整流之后 —— **积压时定稿
  文本被延迟 N×LLM 时间**（见 §P1，这是一个隐藏延迟 bug）。
- **段级重跑冗余**：多句段落重听整段，与句级 batch 高度重叠，且拖慢定稿。

**一个关键有利条件**：前端折叠（`voice_state.rs`）对**乱序 / 迟到事件已经鲁棒** ——
它按 `(paragraph_id, sentence_id)` 键控，迟到 `BatchSentence` 会**刷新**已关闭段落而非清空，
`plain`/`calc` 预览逐级回退（`voice_state.rs:88-113`、`:375-391` 注释明确覆盖"remote batch
~3.5s 可晚于 merge_gap 触发的段落关闭"）。**因此下文所有让事件"变慢 / 乱序"的优化，前端零改动。**

---

## 2. 设计原则

1. **音频热路径只做 VAD + 流式 + 边界决策** —— 一切慢 I/O（batch / 重跑 / LLM）移出到 worker。
   （= P0，`async-batch-design.md`。）
2. **batch 增量优先**：句级 batch 是快速主路径（每句关闭即出文本，喂 live `plain` 预览）；
   段级重跑降为**可选精化**（只长段落做，跨句上下文最有价值）。
3. **LLM 整流去冗余**：定稿时整段**一次**，不再每句整流全段 —— 默认 1 次 LLM/段。
4. **前端零改动**：`AsrEvent` wire 协议 FROZEN，`voice_state.rs` 折叠天然兼容乱序/迟到。
5. **每层独立可灰度**：全部走配置旋钮，可单独开 / 关 / 回退，互不耦合。

---

## 3. 分层方案

> P0 是地基（修 bug）；P1/P2 是成本与延迟优化；P3/P4 是延迟与鲁棒性。除 P0 外，
> 每层都**依赖 P0**（消费循环先变快，后续优化才生效），但彼此**独立**。

### P0 —— batch 异步化（= `async-batch-design.md`，地基）✅ 已实现（2026-08-30）

**问题**：句级 batch（`recognizer.rs:804`）与段级重跑（`recognizer.rs:620`）是消费循环内的
同步调用（远程 ~3.5s），阻塞 VAD / 流式 / `check_settle` → **吞句 bug**（墙钟误切段落）+
流式 partial 冻结。

**方案**（详见 `async-batch-design.md`，此处只摘要）：
- 新增 **batch 工作池**（N 个 std 线程，`AsrProvider: Send+Sync` 天然可并发）；消费循环在
  EOS / settle 只**入队 job**（微秒级），不等待。
- 新内部事件 `SentenceBatchReady` / `ParagraphBatchReady`；pipeline 按**就绪条件**
  （全句 batch 齐 + 重跑齐）触发定稿。
- **Stage2 去状态化**：`finalize_paragraph` 改为**跑一次 LLM**（用全句 best_text），替代
  旧的"读存档、零 LLM"。
- 定稿 = `max(末句 batch, 重跑) + 1 次 LLM`，**不劣于现状**，且 live 流式不再冻结。

**效果**：修复吞句 bug；流式 live 持续流动；消费循环零阻塞。
**前置**：P1/P2/P3 都依赖它（消费循环必须先变快）。

---

### P1 —— Stage2 去冗余（整流成本 + 隐藏延迟 bug）★ 本文新增

**问题**（P0 未解决，且是独立的大头）：
1. **O(n²) token 成本**：`calibrate_paragraph`（`calibrator.rs:102`）每句 EOS 把"全段至今"
   所有句喂 LLM。N 句段落 → N 次调用，总输入 ≈ 1+2+…+N = **N²/2** 句单位。6 句段落 = 21 单位
   （= 6 次单句的 3.5×）。
2. **隐藏延迟 bug**：Stage2 是**单一 worker**，`Batch(1)…Batch(N) → ParagraphEdge` 有序到达
   （`pipeline.rs:237`）。每次 `Batch` 触发一次**阻塞** LLM 调用，`ParagraphEdge`（定稿）排在
   **全部 N 次整流之后**才处理。正常语速下 worker 跟得上（LLM < 句间隔），无感；但**快速说话 /
   LLM 偏慢**时 mpsc 积压，**定稿文本被延迟 N×LLM 时间** —— 用户停顿后迟迟不出最终文本。
3. **P0 之后变成 N+1**：P0 让 finalize 跑 1 次 LLM，若保留每句 live 整流，则 = **N+1 次/段**。

**方案（默认 = 只在定稿整流一次）**：
- **砍掉每句 live 整流**：`Batch` 事件**不再**触发 `calibrate_paragraph`；只在 `ParagraphEdge`
  （= P0 的就绪定稿）时整流一次全段。
- 定稿文本用**全句 best_text**（此时句级 batch 已齐）→ 1 次 LLM，输入 O(N)（= N 单位，
  比现状 N²/2 少 ~N/2×）。
- **效果**：LLM 调用 **N → 1**/段；token **N²/2 → N**；**定稿不再被整流积压延迟**（1 次 LLM
  而非 N 次串行）。
- **代价**：`#asr/calc` 的**渐进**校准预览消失（说话中看不到逐句整流的 calc 文本），calc 文本
  在定稿时一次出现。`#asr`（默认 `plain` 预览）**完全不受影响**（`plain` = batch > 流式拼接，
  句级 batch 增量到达即可）。

**方案 B（可选旋钮 `llm.live_calibrate`，默认关）**：保留 live 整流但**限流** —— 每 ≥T 秒
（默认 2s）或仅在 settle 前最后一句整流一次，喂 `#asr/calc` 的渐进预览。成本 = 1 + ⌈段时长/T⌉
次/段（有上界），兼顾渐进性与成本。实现上在 Stage2 worker 里加一个时间闸门即可。

**推荐**：默认方案（定稿一次），`llm.live_calibrate` 作为可配旋钮给重度 `#asr/calc` 用户。
**注意**：P0 的 Stage2 去状态化（finalize 跑 LLM）已为本步铺路 —— P1 只是把"每句 live 整流"
也关掉 / 限流。

---

### P2 —— 段级重跑降冗余（定稿延迟）★ 本文新增

**问题**：多句段落在 settle 时**重听整段**（`recognizer.rs:620`，~3.5s 远程），
- 与句级 batch **冗余**：每句已单独 batch，重跑主要价值是"跨句上下文重听"，短段落边际收益小；
- **拖慢定稿**：P0 的定稿要等重跑完成（`max(末句 batch, 重跑)`），重跑是长 job（整段音频）。

**方案（泛化现有"单句免重跑"）**：
- 现状：单句段落**复用句级 batch**、跳过重跑（`recognizer.rs:612`）。
- 扩展：**短段落**（≤ `rerun_max_sentences` 句，默认 2；或 ≤ `rerun_max_seconds` 秒，默认 3s）
  **跳过重跑**，定稿直接用**句级 batch 拼接**（`VadParagraph::best_text` 的回退路径已支持）。
- **长段落**才重跑（跨句歧义最多，重听价值最大）。
- **效果**：多数段落（2 句以内）定稿**不再等重跑** → 定稿延迟从 `max(末句 batch, 重跑)` 降到
  `末句 batch`；省掉冗余 ASR 调用。旋钮 `asr.rerun`：`auto`（默认，短段跳过）/ `always` / `never`。

**权衡**：跳过重跑损失一点"整段跨句重听"质量（短段落很小）；`asr.rerun: always` 可完整回退
现状语义。

---

### P3 —— 首字延迟（~1s → 更低）★ 本文新增

**问题**：一句的**首个流式文本**要等：VAD 起音检测（Silero 过阈值需几帧）+ lead-in 补喂
（`LEAD_IN_FRAMES=16` ≈ 0.5s）+ 首次解码（`PARTIAL_EVERY_FRAMES=15` ≈ 0.5s）≈ **1s**。

**方案（自适应解码节奏，低风险）**：
- **句首提速**：会话刚起音（前 ~1s / 前 ~30 帧）用更密的解码节奏（如每 4 帧 ≈ 0.13s），
  拿到第一个字快；句内（稳定后）恢复 15 帧节奏省 CPU。
- 实现：`ActiveSession` 加一个"距起音帧数"计数，切换 `PARTIAL_EVERY_FRAMES`（`recognizer.rs:754`）。
- **可选**：微调 VAD 起音 / lead-in（`threshold` 0.1 已很灵敏；lead-in 0.5s 可降到 0.3s）——
  质量旋钮，需 A/B。

**效果**：首字延迟 ~1s → ~0.3–0.5s；句内 CPU 不增。

---

### P4 —— 鲁棒性（P0 引入异步后必须补齐）

| 项 | 方案 |
|---|---|
| **batch 超时** | 每个 batch job 带独立超时（默认 15s，区别于转发的 300s）；超时 → 结果 `None` → best_text 回退流式（既有 `Option` 契约）。防单 job 永久占 worker。 |
| **必出结果** | batch 池保证**每 job 必发一个结果事件**（`Some`/`None`）→ P0 的"就绪计数"一定能到齐。 |
| **就绪超时兜底** | `ParagraphEdge` 记 deadline（默认 15s）；tick 线程扫描，超期按当前 best_text 定稿（防 job 异常丢失）。 |
| **乱序/迟到** | 顺序不变式见 §5；前端 `voice_state.rs` 已鲁棒（§1 关键有利条件）。 |
| **背压** | P0 后消费循环零阻塞 → ring（10min 容量）永不饱和；batch 池 job 共享 `Arc<PCM>`，不放大内存。 |
| **idle 深度睡眠** | `running=false` 消费循环退出，但 batch 池**继续排空**在途 job（发结果到 Stage2）；结果对应已暂停段落，前端折叠无害。 |

---

## 4. 优化后拓扑

```
omni-scout /audio (TCP)
   │  ingest 线程（scout → AudioRing, Condvar 唤醒）
   ▼
AudioRing
   │
   ▼
消费循环（aura-pipeline 线程）—— 只做 VAD + 流式 + 边界决策，永不阻塞
   │  EOS:    句级 batch job 入队 ┐        emit StreamFragment(流式定稿) + Batch{句, batch=None}
   │  settle: 段级重跑 job 入队 ─┤(短段跳过,P2)  emit ParagraphEdge
   │        (P3: 句首加速解码)   │
   ▼                              ▼
   ┌──────────── batch 池（N std 线程, 并行, 每 job 必出结果 + 超时）────────────┐
   │  recognize() → SentenceBatchReady{sid,text} / ParagraphBatchReady{pid,text}   │
   └───────────────────────────┬──────────────────────────────────────────────────┘
                               ▼  （与 Batch/ParagraphEdge 汇入同一 Stage2 mpsc，乱序安全）
Stage2 worker（aura-stage2 线程）
   │  SentenceBatchReady → 累积句 batch（P0）
   │  Batch            → (P1: 默认不整流；live 限流可选)
   │  ParagraphEdge    → 就绪(全句 batch 齐 [P2: 短段免重跑]) → LLM 整流一次 → 定稿
   ▼
Pipeline: record_final（PCM + 三层文本）+ TurnEvent → SSE /api/asr_stream
   ▼
前端 voice_state.rs 折叠（按 (pid,sid) 键控，乱序/迟到鲁棒）→ 🎙 候选 / #asr / #asr/calc
```

**线程清单（优化后）**：`aura-stage1-ingest`（采集）+ `aura-pipeline`（消费循环，**零阻塞**）
+ **batch 池**（N worker，新）+ `aura-stage2`（LLM，1 worker）+ `aura-socket`（tokio SSE）
+ 1 个轻量 tick 线程（P4 就绪超时兜底）。

---

## 5. 正确性不变式（异步化后必须守住）

1. **同句的 `Batch` 先于其 `SentenceBatchReady`**（EOS 发 Batch，batch 完成后发 Ready）。
2. **`SentenceBatchReady` 之间任意顺序安全**：pipeline 按 `sid` 累积，前端按 `(pid,sid)` 折叠。
3. **定稿 `ParagraphCalibration` 必在该段所有 `BatchSentence` 之后**：定稿本就等
   `ready == expected`（全句 batch 齐），天然满足 —— 这正是"定稿用最佳文本（末句 batch 不缺失）"
   的关键。
4. **`BatchParagraph` 允许与部分 `BatchSentence` 交错**（池并行下重跑先完成）：前端已关闭段落
   的 `plain_preview` 优先 `batch_paragraph`，不受影响。
5. **跨段落按 `pid` 隔离**：旧段落的重跑/定稿事件不污染 `current_paragraph` 预览。
6. **`StreamFragment` 恒 inline**（不经池、不经 Stage2）：live 显示不受 batch/LLM 延迟影响。

---

## 6. 成本 / 延迟对比（N 句段落；远程 batch ~3.5s，LLM ~1.5s）

| 维度 | 现状 | +P0 | +P1 | +P2 |
|---|---|---|---|---|
| 消费循环阻塞 | **是（吞句 bug）** | 否 | 否 | 否 |
| 流式 live 预览 | batch 期间**冻结** | 持续流动 | 持续 | 持续 |
| batch 调用/段 | N(句) + 1(重跑) | N + 1 | N + 1 | **短段 N / 长段 N+1** |
| LLM 调用/段 | **N（O(n²) token）** | N + 1 | **1** | 1 |
| LLM 输入 token | ≈ N²/2 | ≈ N²/2 + N | **≈ N** | ≈ N |
| 定稿延迟（相对 settle） | 积压时 **N×LLM** | max(重跑,末句batch)+LLM | **LLM×1** | **短段 LLM×1 / 长段 max(重跑,末句)+LLM** |
| 首字延迟 | ~1s | ~1s | ~1s | ~1s →（P3）~0.3–0.5s |
| `#asr/calc` 渐进预览 | 有（慢/可积压） | 有 | **默认无（定稿出现）/ 可选限流** | 同 |
| `#asr`（默认 plain） | 正常 | 正常 | **不受影响** | 不受影响 |
| 吞句 bug | **有** | **无** | 无 | 无 |

**一句话收益**：P0 修 bug；P1 把 LLM 成本从 N² 降到 N、并消除定稿积压延迟；P2 让多数段落定稿
更快且少一次冗余 ASR；P3 把首字延迟砍半。**全部收敛在 `aura-core`，前端 / daemon / wire 零改动。**

---

## 7. 落地顺序与灰度

1. **P0 先行**（`async-batch-design.md`）：修吞句 bug + 流式解冻。这是其余优化的前置。
   - 灰度：batch 池 N 可配（0 = 回退现状同步路径）。
2. **P1 默认关 live 整流**：改 Stage2 worker 的 `Batch` 臂（不触发 LLM），`finalize` 已跑 LLM
   （P0）。成本立省，`#asr` 无感。
   - 灰度：`llm.live_calibrate: off`（默认）/ `throttle:T` / `per_sentence`（回退现状）。
3. **P2 短段免重跑**：`emit_paragraph_edge` 的"单句免重跑"泛化为"短段免重跑"。
   - 灰度：`asr.rerun: auto`（默认）/ `always` / `never`。
4. **P3 首字加速** + **P4 鲁棒性**（batch 超时 / tick 兜底）：微调 + 防御，随 P0 一并落。

每步独立可回退（配置旋钮），互不耦合；建议 P0+P1 一起上（一个 PR 内完成 Stage2 去冗余），
P2/P3/P4 随后。

---

## 8. 开放问题 / 后续（本轮不做）

1. **live 限流整流的最优 T**：需按 LLM 延迟 + 典型段时长 A/B（2s 起）。
2. **重跑作为"异步精化"**：定稿先按句级 batch 出，重跑到达后**刷新**归档 + 可选重发 ——
   定稿更快，但引入"定稿后文本被刷新"语义，需前端配合，单独设计。
3. **batch 池自适应 N**：按在途 job 数 / 实测延迟动态调。
4. **流式引擎 x-asr**（2026 百万小时 zh-en，自带标点）：质量/延迟权衡，可能同时改善首字与
   句级文本质量（`recognizer.rs:172` 已留 `with_stream_engine` 扩展点）。
5. **`AudioStore` Arc 化**（`Vec<i16>` → `Arc<Vec<i16>>`）：让 job / 段落 / 归档共享 PCM，
   评估对 `/api/audio`、归档 WAV 路径的影响。

---

## 附：一页速览

| 层 | 解决什么 | 主要改动（crate） | 灰度旋钮 | 前端 |
|---|---|---|---|---|
| **P0** | 吞句 bug + 流式冻结 | batch 池 + 新事件 + Stage2 去状态化（`aura-core`） | `asr.batch_pool: N`（0=回退） | 零改动 |
| **P1** | LLM O(n²) + 定稿积压延迟 | Stage2 `Batch` 臂不整流 / 限流（`aura-core`） | `llm.live_calibrate` | 零改动 |
| **P2** | 冗余重跑 + 定稿延迟 | 短段免重跑（`recognizer.rs`） | `asr.rerun` | 零改动 |
| **P3** | 首字延迟 ~1s | 自适应解码节奏（`recognizer.rs`） | `asr.first_word_fast` | 零改动 |
| **P4** | 异步化鲁棒性 | batch 超时 / tick 兜底（`aura-core`） | 超时阈值 | 零改动 |
