# round15 — 链式预测 × 魔法异步:部分重算 + 节流/防抖

需求:`abc'#asr'#translate` 场景,语音不断进入时 ——
上游文本段(A、B、C)不重算;从语音段起的下游流水线重新预测;
为防面板被连发刷新打搅,加节流与防抖。

## 一、现状缺口(勘察结论)

- `eval_upstream`(magic_flow)每次按键/每次求值全量递归左折叠,
  无缓存 —— abc 段被反复重新打分/重新 spawn 命令求值。
- `magic_tick` 异步事件只服务**非链式** Snippet 态(活跃成员 = 最后段);
  链式模式下语音更新(`#asr` 段的 SharedTranscript 变化)不会触发
  下游(`#translate`)重预测 —— 组合场景完全无异步刷新,更无流控。

## 二、设计:ChainFlow(Session 内的链式流控状态机)

新类型在 `fsm/chain.rs`(纯状态机;时钟由调用方注入,测试造任意时间序列):

```rust
pub(crate) struct ChainFlow {
    cache: HashMap<String, Vec<String>>,  // 上游折叠缓存(内容寻址)
    seen: String,                          // 滚动变化检测基线
    last_refresh_ms / source_changed_ms,   // 节流 / 防抖起点
    pending, primed,
}
```

### 2.1 部分重算:缓存命中即零重算

- `eval_upstream` 拆两层:外层查 `chain_flow.cached(upstream_buf)`,
  命中直接返回;未命中走 `eval_upstream_uncached` 并 `store` 回填。
- **内容寻址**:key = upstream buffer 文本。`abc'#asr'#translate` 反复
  刷新时,abc 前缀段每次命中缓存 —— 只有语音变化之后真正的重算发生;
  用户编辑 abc → key 变化,天然失效,无失效逻辑负担。
- 上限 32 条,超限整体清空(重建即可,无 LRU 复杂度)。

### 2.2 异步接合:magic_tick 分流

```
AsyncEvent::MagicTick → stage2 magic_tick
    ├─ 链式命令态(is_chain_command)→ chained_flow_tick   ← 新
    └─ 单命令态 → 原 tick 逻辑(不变)
```

`chained_flow_tick`:
1. 源指纹 = 上游折叠候选序列(`eval_upstream`,缓存命中 → abc 零成本);
2. 指纹喂 `ChainFlow::gate(now)`:
   - 首次观察 → 记基线,Quiet(键驱预测已展示过该源);
   - 无变化且无 pending → Quiet(面板不被语音噪声打搅);
   - 变化 → 重置防抖起点,**流式连发期间一直推迟,停顿才放行**;
3. Fire → 重走 `query_chained_magic`(缓存使上游段零成本,
   最后段命令全量重预测 → `#translate` 拿到新语音文本)。

### 2.3 闸门时序(测试钉死)

```
DEBOUNCE_MS = 250(防抖:源静默此时长才放行)
THROTTLE_MS = 150(节流:两次刷新最小间隔)

t=0   源"A"首次观察 → Quiet(记基线)
t=100 源"A→B"      → Suppress(防抖起点)
t=200              → Suppress(停顿 100 < 250)
t=360              → Fire(停顿 160 ≥ 防抖)
t=400 源"B→C"      → Suppress(距上次刷新 40 < 节流 150)
t=700              → Fire(补刷:防抖早过、节流已过期)
t=800 无变化       → Quiet
```

`reset()`(提交/重置)时 `chain_flow.clear()`:源已消费,缓存与闸门归零。

## 三、边界与不变量

- 全部状态在 SessionState(`chain_flow` 字段),壳(engine)零改动 ——
  事件驱动,stage2 门面内聚;
- 闸门只管**异步刷新**:键驱预测(用户在打字)永不经过闸门;
- `eval_upstream` 改 `&mut self`(缓存写入);调用点均在 stage2 方法内,
  无借用冲突。

## 四、验证

- 新增 5 个 ChainFlow 单元测试(时序/基线/缓存寻址/上限/复位),全绿;
- 全套 209 passed;clippy 双 crate 0;评测持平 98.0 / 99.4。
