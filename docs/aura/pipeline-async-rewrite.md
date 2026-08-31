# aura Pipeline 异步化重写 — Finalizer 状态机 → per-paragraph async 任务(round12)

> 创建: 2026-08-31。round11 实测后用户拍板:**用 tokio 改写 aura 后端
> Pipeline,通过异步函数简化时序控制逻辑**,与 ime-core round10 建立的
> 异步范式(阻塞线程产事件 → channel → 专用 current_thread runtime
> `select!` 主循环 → 单点发射)同构。
>
> 本文档覆盖 `issues-asr-transcript.md` 的 S3 后续:工作区中 round11
> 未提交改动(S1/S2/修订/S3/丢弃留痕)与本次重写一并构成交付。

## 一、现状与病灶

`aura-core/pipeline.rs` 现状(三线程 + 手写状态机):

```
aura-stage1-ingest  s1.run_ingest()                 阻塞,音频 → ring
aura-pipeline       s1.run(cb)                      阻塞,流式/VAD → Stage1Event
                      StreamFragment → inline 直发 on_turn   ← 发射源 1
aura-batch          s1.run_batch_worker(batch_rx)   阻塞,batch ASR → tx
aura-stage2         Finalizer(rx)                   阻塞,就绪门 + LLM → on_turn
                                                    ← 发射源 2
```

病灶:
1. **`on_turn` 两个并发调用源** → broadcast 到达顺序竞态(round11 Bug 2 根因);
2. **Finalizer 就绪门状态机**(`HashMap<ParagraphId, PendingFinal>` +
   ready/expected/para_done 计数 + `try_finalize` 门,~300 行)——"等全部
   句 batch 齐 + 段重跑齐"用手写计数表达,时序不变式靠注释维护;
3. batch job 依赖 s1 内部 enqueue(`batch_tx`),worker 常驻线程。

## 二、目标架构

```
保留(阻塞,专线;recognizer 识别逻辑零改动):
  aura-stage1-ingest  s1.run_ingest()
  aura-pipeline       s1.run(cb) —— cb 只往 tokio unbounded_channel 产 Stage1Event

新增(tokio current_thread runtime,专用线程 "aura-pipeline-async",
     与 ime-core IoThread 同构):
  主循环 select!(唯一 on_turn 调用者 —— 发射单点):
    ev = s1_rx.recv() => match ev {
      StreamFragment → emit                              (直通,低延迟)
      Batch          → 段句集累积 + 逐句 spawn 句任务      (EOS 触发)
      ParagraphEdge  → emit(ParagraphClosed) + spawn 段任务
    }
    ev = turn_rx.recv() => emit(ev)                       (任务产出回传)

句任务(EOS 触发,spawn_blocking):
  recognize_once(audio_store.concat(&[audio_id])) → turn_rx{BatchSentence}

段任务(ParagraphEdge 触发):
  join 句任务 handles { 每完成: 累积 best_text + live 联合整流
                        → turn_rx{SentenceCalibration} }     ← 就绪门 = join!
  多句 → recognize_once(paragraph.pcm) → turn_rx{BatchParagraph}
  spawn_blocking(s2.finalize_paragraph) → storage.record_final
  → turn_rx{ParagraphCalibration}
```

### 时序语义(与现状对比)

| 保证 | 现状 | 重写后 |
|---|---|---|
| 边界先于下一段 | ParagraphClosed 与 s1 回调同线直发 ✓ | **更强**:所有事件单点 emit,s1_rx FIFO 内自然有序 |
| 段内 calibrated 严格在 batch 后 | Finalizer 注释维护的不变式 | **结构保证**:live 整流在段任务 join 循环里,串行于句 batch 完成 |
| 定稿在全部句 batch 后 | ready == expected 计数门 | **join! 语义** |
| 定稿 vs 新段流式 | 乱序(物理) | 乱序(物理,不变)——客户端 id 修订兜底 |

### 删除物

- `Finalizer` / `PendingFinal` / `sentences: HashMap` / 就绪计数 / `try_finalize`
- `aura-batch` worker 线程 / `run_batch_worker` 调用(batch 由任务自建)
- Stage1Event 的 `SentenceBatchReady` / `ParagraphBatchReady` 消费路径
  (枚举变体保留 —— s1 的 `batch_jobs=false` 下不再产生,但 wire/类型不破坏)

## 三、recognizer.rs 的最小改动(识别逻辑零动)

1. `recognize_once` 私有 → `pub`(settle 任务直调;实现一行不动);
2. `Stage1Config` 增 `batch_jobs: bool`(默认 `true` 保持向后兼容):
   `false` 时 s1.run **不投** `BatchJob`(两处 enqueue 点加 guard)——
   batch 由 pipeline 任务自建,避免 worker 与任务双跑双倍算力。
   `Pipeline::assemble` 置 `false`。
3. 句级 PCM 时效:句 clip 在 EOS(`Batch` 事件,`audio_store.insert` 后)
   → ParagraphEdge(evict)之间存活;句任务在 `Batch` 事件处理时立即
   `concat(&[audio_id])` 取 Arc —— evict 只删 AudioStore 条目,Arc 已存活 ✓。

## 四、接口与引用面

| 项 | 变化 |
|---|---|
| `Pipeline::new(s1, s2, batch_rx)` | batch_rx 参数移除(示例 stage12_live.rs 随改);`assemble` 内部置 `batch_jobs=false` |
| `Pipeline::spawn(running, resume, on_turn)` | 签名不变;内部 = s1 阻塞线程 + tokio 线程 block_on 主循环 |
| `TurnEvent` | 不变(ParagraphClosed 已在 round11 S3 加入) |
| `apps/audio-aura/main.rs` | 回调不变(broadcast.send 本就 sync);`TurnEvent` 映射已含 ParagraphClosed |
| `Cargo.toml` | aura-core 增 `tokio`(workspace 版,features: rt/time/sync/macros) |

## 五、分步提交

| 步 | 内容 |
|---|---|
| 1 | round11 收尾批:aura-agent(S1 分帧/transcript/丢弃留痕)+ ime-core 接线 + docs(工作区既有改动) |
| 2 | recognizer.rs 最小改动(pub recognize_once + batch_jobs 开关) |
| 3 | pipeline.rs 异步化重写 + examples 随改 |
| 4 | 全仓回归:aura-daemon 编译、calibrate_bench/stage12_live 编译、ime-core/swift-ime 全绿、clippy 零警告 |

## 六、验收

- aura-daemon `--features` 全量编译零警告;
- `stage12_live` / `calibrate_bench` 示例编译通过;
- 行为对照:流式直通延迟不回退(StreamFragment 仍单跳直发);
  定稿三级占位/替换(BatchSentence → BatchParagraph → ParagraphCalibration)
  时序不变式成立;边界 ParagraphClosed 保序;
- 实机(用户):连续说话分段,候选不丢单、不重复、顺序正确。
