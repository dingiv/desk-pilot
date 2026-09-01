# aura 排障手册(简略)

> 合并自:issues-asr-transcript / pipeline-async-rewrite / pipeline-optimization /
> async-batch-design 的调查与修复记录(2026-08-31 整理)。历史细节见 git;
> 现行架构见 [architecture.md](architecture.md) / [pipeline.md](pipeline.md)。

## 一、留痕对表(定位丢事件/时序问题的第一手段)

两端各打一行同词汇日志,diff 即定位缺口:

| 端     | 日志                                                           | 位置                                            |
| ------ | -------------------------------------------------------------- | ----------------------------------------------- |
| server | `emit→前端 event=…`(六种事件单行,p=时间戳段 id,即段落创建时刻;**流式事件 debug 级,其余 info** —— 对表时开 debug) | `pipeline/tasks.rs` `emit_turn`/`describe_turn` |
| client | `前端←event event=…`(同词汇)                                   | `aura-agent/client.rs` `describe_event`         |

- batch/纠偏调用各有 info 级 `start/end/耗时` 一条(`batch[sentence]`/`batch[paragraph]`/
  `纠偏[sentence]`/`纠偏[paragraph]`),墙钟 HH:MM:SS.mmm 可直接与 emit 序列对表;
- `hello` 握手 ack 静默跳过(非契约不匹配);
- SSE 断连/lagged 有 warn;内部重连成功后自发 `Resync`(reset + `/api/results`
  全量对账 —— 广播无回放,这是唯一补历史通道)。

## 二、症状 → 根因速查(均为已修,防复发)

| 症状(客户端/日志所见)               | 根因                                                                 | 修复轮                                                                          |
| ----------------------------------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| 间隔 1–3.5s 后首句被吞              | batch 同步阻塞消费循环,墙钟越过 merge_gap 误切段                     | round12:batch 任务化                                                            |
| 首选 batch 后退回流式               | 起音落在 merge_gap 截止前的 partial 盲区,被误切;关段后迟到 SF 未忽略 | round15:`speech_pending` 边际 + 客户端忽略已关段 SF                             |
| 定稿(PCal)发出流式文本而非 batch    | 段落实体的句集快照未回填 join 结果                                   | round16b:join 回填写回                                                          |
| daemon 崩 `Cannot drop a runtime …` | `reqwest::blocking`(HttpAsr/重跑)裸跑在 async 任务                   | round17:重跑/归档包 `spawn_blocking` + 15s 兜底(**PCal 必发**)                  |
| SC(纠偏)内容是流式、早于 batch      | SC 触发点在 Batch 事件(EOS 时刻),batch 尚未回                        | round17b:触发点移到 **BS 到达**,段内链式串行                                    |
| SSE 每 ~30s 掐流、UI 闪断           | `AuraClient` 的 `.timeout(30s)` 覆盖整个响应生命周期                 | round19:SSE 专用 client(仅连接超时,无总超时)                                    |
| 同段第二句流式期间 UI 不刷新        | SC 是快照,遮住过界新句                                               | round20/20b:`sentence_calibration` 自带 `segment_id`(覆盖上界),前端 SC+尾巴续接 |
| 前端出现幽灵段/段落错位             | 随机段 id + 预测键                                                   | round13:时间戳 id(严格递增)+ 起音即开段                                         |
| 同一句 partial 在旧段、定稿却进新段(日志:旧段 PC/PCal 插在新句事件中间) | settle 用 end−PCM 反推 start_s(偏晚 ~0.5s → 间隔虚增),与起音判定矛盾 | round26:start_s/settle 统一用起音翻转墙钟 |
| 流式会话幻觉复读卷入下一句          | 微弱音频悬置会话 35s                                                 | 8s 停滞看门狗重置                                                               |
| 中文缺字出现 U+FFFD                 | SSE 逐 chunk lossy UTF-8 解码                                        | round11:跨 chunk 缓冲解码                                                       |
| 长流丢事件                          | (见上 SSE 掐流)                                                      | round19                                                                         |

## 三、round 简表

| 轮         | 主题                                                                                                                                                                                           |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 11         | SSE 解码/事件归位(见上表)                                                                                                                                                                      |
| 12         | Pipeline 异步化:Finalizer 状态机 → per-paragraph 任务 + select! 单点发射                                                                                                                       |
| 13         | 时序错位修复:时间戳段 id、起音开段、空段 GC、只投 just-closed 句                                                                                                                               |
| 14/14b     | 消费循环全异步(Notify,R5 关闭轮询)                                                                                                                                                             |
| 15         | 起音盲区边际(VOICE_SETTLE_MARGIN)+ 客户端忽略已关段 SF                                                                                                                                         |
| 16/16b     | 统一发射留痕;PCal 回填写回                                                                                                                                                                     |
| 17/17b/17c | 重跑 spawn_blocking+兜底;SC 触发点=BS;纠偏输入双通道(both)                                                                                                                                     |
| 18/19      | 前端级联折叠(§8)+ 接收留痕;SSE 专用 client + hello + Resync                                                                                                                                    |
| 20/20b     | SC 覆盖上界走协议(segment_id)                                                                                                                                                                  |
| 21/21b | 流式模型独立 tokio::task;run 改固有 async fn |
| 25/26 | 日志分级(流式 debug / batch·纠偏 info + 起止墙钟与耗时);settle 量尺统一(起音墙钟),修"同句中途换段" |
| 22–24      | 模块重构:front→vad、pipeline/ 文件夹化(consume/recognizer/tracker/stream/tasks)、死路径清除(batch worker/SentenceBatchReady)、select! 臂处理器化(Ctx/Turns)、通道简化(流式一对通道,回执同通道) |

## 四、常见误报(不是 bug)

- 级联跳变(流式→batch→纠偏)= 两级识别范式的固有形态(渐进精化);
- 单句段落没有 `batch_paragraph`(整段重跑)—— 复用句级 batch;
- `llm.backend: disable` 时 `sentence_calibration`/`paragraph_calibration` 承载原文
  (PassThrough 恒等),route_ms≈0。
