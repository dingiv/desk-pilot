# VadSegment / VadWindow 状态机模型（已落地 2026-08-17）

> 状态：✅ **as-built**——本设计已于 2026-08-17 实现并通过测试（Rust 侧全量切换；
> 前端三处迁移待做，见 `stages.md` 迁移状态节）。流程细节以 `stages.md` 为准，
> 本文保留设计动机与裁决记录。

## 动机（为什么推翻"就地修改"范式）

1. **内存**：录音 PCM 今天在 `Utterance`/`MergeAccum`/`Storage` 间整块克隆；改由专门
   store 持有，实体只存 id。
2. **计算**：batch 识别从"每个 EOS 对**累积**音频重跑"（n 段 utterance 总量 O(n²) 音频）
   变为"每段一次 + 窗口定稿拼接重跑一次"（O(n)）。
3. **语义**：`VadSegment` / `VadWindow` 扶正为一等实体，替代隐式的 `MergeAccum`；
   事件从"同 seq 就地更新（修改范式）"变为 **append-only 片段 + 边界标记**。
4. **Stage2**：从"单 utterance 整流"升级为"**窗口内多句联合整流**"——跨句上下文
   改善同音字、标点、连贯性。

## 实体模型

```
AudioStore                    专门的录音数据管理模块（id → PCM[i16]）
  │  VadSegment / VadWindow 只持有 id，不再各自克隆 PCM
  ▼
VadSegment                    VAD 间隔切出的原子录音片段
  · id: u64
  · audio_id: AudioId         // 真实 PCM 在 store 里
  · start_s / end_s           // SOS/EOS 墙钟时间戳
  · streaming_text            // 本段流式识别结果
  · batch_text: Option<String> // 本段 batch 结果；None 合法（batch 依赖远程网络，可能失败）

VadWindow                     merge window 内多个 VadSegment 的组合
  · id: u64
  · segment_ids: Vec<SegmentId>
  · 窗口级聚合：
      streaming = 各段 streaming_text 直接拼接（零成本）
      batch      = 拼接各段录音后重新进 batch 模型识别一次（跨段上下文，权威）
```

两个时间参数（不变，仍是 `aura.yaml` 的 `vad.min_silence` / `vad.merge_gap`）：
- **VAD 间隔**（min_silence）：决定 VadSegment 的边界——每一段静音间隔 = 一个真实片段。
- **merge window**（merge_gap）：决定 VadWindow 的边界——大中断 = 窗口关闭。

## Stage1 状态机

```
scout 音源到达 ──► 新建 VadSegment，喂流式识别，驱动 partial 生成
     │
VAD 间隔 (min_silence)
     └──► 对该段触发一次 batch 识别，结果打包进 VadSegment（可失败 → None）
          emit Batch(segment) ──────────► Stage2
     │
merge window（大中断 / 超时）
     └──► 各段拼接 → 窗口级 batch 重跑 → VadWindow 定型
          emit WindowEdge(window) ──────► Stage2
```

## Stage2 行为

- **收到 Batch**：把**当前窗口内所有 VadSegment** 的文本（batch_text 优先，
  streaming_text 兜底）喂给文本模型**联合整流**——多个句子一起整。
- **收到 WindowEdge**：**移动左边界**——该窗口定稿（联合整流结果即最终文本），
  后续整流不再包含其片段。

## 与现状的差异

| 维度 | 现状（stages.md） | 本模型 |
|---|---|---|
| 原子实体 | 隐式（VadEvent.pcm + MergeAccum 时间戳） | `VadSegment` 一等实体（id/时间戳/双路文本） |
| 音频持有 | `Utterance.pcm` 整块随事件克隆 | `AudioStore` 按 id 管理，实体持 id |
| batch 输入 | 每 EOS 跑**累积** PCM（O(n²)） | 每段跑自己的 PCM + 窗口定稿拼接重跑（O(n)） |
| batch 失败 | 空 text，回退流式 | `batch_text: Option` 显式建模 |
| 流式会话 | 横跨整个 utterance，定稿才 finalize | 段级（待定：见 D1） |
| Stage2 输入 | 单 utterance 的 route_text | 当前窗口**全部片段**联合整流 |
| 事件范式 | Batch/MergeBatch + 同 seq 就地更新（修改范式） | Batch/WindowEdge + append-only（边界范式） |
| Stage2 上下文 | ContextWindow（已禁用） | 窗口本身即上下文；WindowEdge 滑动左边界 |

## 已决决策（2026-08-17 拍板，均已实现）

- **D1 流式会话粒度 → 段级会话**：每个 VadSegment 独立开流式会话，EOS 定稿。
  接受段边界编码器上下文丢失（段首字可能略差）；每段有完整流式结果，拼接即窗口流式。
- **D2 说话中实时纠偏 → 砍掉**：Batch 只在 VAD 间隔触发；说话中 UI 只显示 raw
  partial，纠偏文本在首个 VAD 间隔后出现（~1s 滞后）。省掉每秒一次的 LLM 调用。
  （勤快 Stage2 的 `STREAM_CALIBRATE_INTERVAL` 路径删除。）
- **D3 WindowEdge 产出 → 窗口级 Final**：一个窗口定稿一条（窗口内多句联合整流的
  完整文本）。UI 时间线：段实时生长 → 窗口关闭收拢为一条定稿。
- **D4 迁移策略 → 直接替换**：一次性 breaking——executor / composer / calibrator /
  daemon / aura-agent SDK 已同步切换；**前端三处（swift-ime / geek-familiar /
  devtools）按用户指示暂缓**，作为后续独立迁移任务。

实现补充裁决（落地时定）：
- **Stage2 无状态**：Batch 事件每次携带当前窗口全部段（载荷即窗口），无内部缓冲。
- **死代码清除**：`Decision`/`parse_decision`/`ContextWindow`/`AudioChunk` 删除，
  Stage2 校准直接返回 `String`。
- **窗口 PCM**：settle 拼接一次为 `Arc<Vec<i16>>` 挂在 VadWindow 上（窗口 batch 与
  daemon 落盘共用），store 随即 evict。
