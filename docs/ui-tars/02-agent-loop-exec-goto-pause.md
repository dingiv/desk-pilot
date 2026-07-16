# 02 · Agent 循环 / 回合制 / goto / pause / 流式

> 映射 VRover：`@vrover/agent` 的 `runAgent()`（`packages/agent/src/core.ts`）+ **正在做的 `Agent` 接口**（`types.ts` 里的 sketch：`exec`/`goto`/`pause` + `core.ts` 的 `TaskImpl`）+ `packages/agent/src/context.ts`（`pruneForModel` 滑动窗口）。
>
> 对照对象：`multimodal/gui-agent/agent-sdk`（`GUIAgent` 类）+ `multimodal/tarko/agent`（`@tarko/agent` 通用 agent 框架：`AgentRunner`/`LoopExecutor`/`AgentEventStream`/`MessageHistory`/`ExecutionController`）+ `apps/ui-tars`（桌面端 `runAgent` 服务）。

VRover 现状：`runAgent(opts)` 是**一次性批处理函数**——for 循环 observe→think→act 跑到 `done`/`max_steps`/`error` 返回 `TaskResult`；`history: Message[]` 全量真源；无中断、无流式、无回合制、无历史分支。

---

## TODO 2.1 — `ExecutionController` + `AbortSignal` 贯穿：`Agent.pause` 的直接范本 `[中断]` · **P0**

- **是什么**：独立 `AgentExecutionController` 管 `AgentStatus`（IDLE/EXECUTING/ABORTED/ERROR）+ **防并发**（`beginExecution()` 执行中再开就抛）+ `abort()`；`AbortSignal` 从 UI 一路传到 `LoopExecutor` 每轮检查。桌面端 `GUIAgent` 还有 **`pause/resume/stop`**：`pause` 用 **Promise 阻塞**（`resumePromise`，**不靠定时器轮询**），`stop/abort` 置状态跳出。
- **VRover 缺口**：`runAgent` 完全无中断——这正是 `Agent.pause` 要补的。
- **待办**：给 `Agent` 加 `ExecutionController`（状态机 + AbortController + 防并发）；`pause` 用 Promise 阻塞式（轮询点放每轮 observe 前 + act 前，保证及时响应）；abort 错误**不重试**直接 bail。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/agent/src/agent/execution-controller.ts#beginExecution` / `#abort`
  - `UI-TARS-desktop/multimodal/tarko/agent/src/agent/runner/loop-executor.ts#executeLoop`（每轮 `abortSignal?.aborted` 检查）
  - `UI-TARS-desktop/packages/ui-tars/sdk/src/GUIAgent.ts#pause` / `#resume` / `#stop`（`isPaused`/`resumePromise` 阻塞式暂停）
  - `UI-TARS-desktop/apps/ui-tars/src/main/ipcRoutes/agent.ts#stopRun`（IPC 层 `abortController.abort()` + 清理覆盖层）

## TODO 2.2 — 流式 / 非流式双模式（同一循环，两套出口） `[流式]` · **P0**

- **是什么**：`AgentRunner.execute()`（非流式，返回最终事件）与 `executeStreaming()`（返回 `AsyncIterable<Event>`）**底层共用一个 `LoopExecutor`**，靠 `StreamAdapter` 把事件流转成异步可迭代对象；循环跑在后台，事件实时推到 stream。
- **VRover 缺口**：`CompleteFn` 今天非流式，`handleResponseChunk` 是空壳。这是 `Agent` 流式的直接参考。
- **待办**：把 `Agent.exec` 设计成 `exec(msg): AsyncIterable<AgentEvent>`（流式）+ 内部可聚合为 `Promise<TaskResult>`（非流式），底层 `runAgent` 循环不变；`handleResponseChunk` 接 LLM 流式 chunk → emit 事件。流式签名细节见 [04](./04-llm-provider-streaming-mcp.md)。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/agent/src/agent/agent-runner.ts#execute` / `#executeStreaming`
  - `StreamAdapter`（`createStreamFromEvents` / `completeStream` / `abortStream`，同文件）

## TODO 2.3 — 单一事件流真源 + 独立「转消息」层（`AgentEventStream` + `MessageHistory`） `[history/解耦]` · **P1**

- **是什么**：`AgentEventStream` 把**一切**（user/assistant 消息、tool_call、tool_result、环境输入、system、streaming 分块）都建模成**一等事件**追加；`MessageHistory.toMessageHistory()` 独立负责「事件流 → 喂 LLM 的 messages」转换 + 图片数量裁剪（`getImagesToOmit`，保留最新 N 张、旧的换文本占位）。
- **VRover 现状**：`history: Message[]` 已是全量真源，`pruneForModel`（`context.ts`）已做滑动窗口压缩 + 截图封顶——**思路已对齐**。差距：VRover 的事件颗粒度是「Anthropic 消息块」，tarko 是更细的「事件流」（streaming/system/tool 都是一等公民），更利于流式渲染与快照。
- **待办（可选增强）**：若 `Agent` 要支持流式 UI / 快照回放，考虑把 loop 内部状态升级为事件流（observe/think-delta/act/result 都是事件），`pruneForModel` 退化为「事件流 → messages」投影。**不急**——VRover 现有 `pruneForModel` 已满足「发给模型的窗口」。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/agent/src/agent/message-history.ts#toMessageHistory`
  - `UI-TARS-desktop/multimodal/tarko/agent/src/agent/message-history.ts#getImagesToOmit`（图片封顶，对照 VRover `capImages`）

## TODO 2.4 — `LoopExecutor` 三态终止 + `onBeforeLoopTermination` hook `[循环/终止]` · **P1**

- **是什么**：终止三因：达到 `maxIterations` / 主动终止（`finalEvent`，如无 tool_calls 即最终答案）/ 外部 abort；并有 `onBeforeLoopTermination` hook 让**高阶 agent 可否决终止**（多 agent 协作用）+ `onEachAgentLoopEnd` 每轮 hook。
- **VRover 缺口**：`done/max_steps/error` 硬编码在 `runAgent` 循环里，无 hook。
- **待办**：给 `Agent` 加循环 hook（`onBeforeStep`/`onAfterStep`/`onBeforeTerminate`），为 `goto`（分支后继续）、walker（D8 已知边直行）、多 agent 留扩展点；与 D8（动态工具注入）呼应。
- **参考出处**：`UI-TARS-desktop/multimodal/tarko/agent/src/agent/runner/loop-executor.ts#executeLoop`

## TODO 2.5 — 分层重试 + 错误分类 + abort 不重试 `[错误/降级]` · **P1**

- **是什么**：桌面 `GUIAgent.run` 用 `asyncRetry` 分别包 **截图**（默认 5 次）、**模型调用**（5 次）、**execute**（1 次）；错误分类成 `SCREENSHOT_RETRY_ERROR`/`INVOKE_RETRY_ERROR`/`EXECUTE_RETRY_ERROR`；**abort 错误不重试直接 bail**；截图连续失败达阈值抛 `SCREENSHOT_RETRY_ERROR`。
- **VRover 缺口**：`runAgent` 无重试，LLM/截图/执行任何一处抖动就直接 `error` 终止。
- **待办**：在 `Agent.exec` 单步内加分层重试（截图/模型多试、执行少试），区分 abort（不重试）与瞬时错误（重试）。
- **参考出处**：
  - `UI-TARS-desktop/packages/ui-tars/sdk/src/GUIAgent.ts#run`（`asyncRetry` 包装三处）
  - `UI-TARS-desktop/packages/ui-tars/sdk/src/types.ts#GUIAgentConfig`（`retry.{screenshot,model,execute}.maxRetries`）

---

## 关于 `exec`（回合制）与 `goto`（历史分支）

- **`exec` 回合制**：tarko 的 `GUIAgent` 是**类式但一次性 `run(input)` 跑到底，非回合制**。所以 VRover 的 `exec`（每次一条新用户消息跑一轮 think→act）是**净新设计**，tarko 仅提供「类式 SDK 形态」参考。
  - 出处：`UI-TARS-desktop/multimodal/gui-agent/agent-sdk/src/GUIAgent.ts`；桌面端封装 `UI-TARS-desktop/apps/ui-tars/src/main/services/runAgent.ts#runAgent`。
- **`goto` 历史分支**：**tarko 完全没有 goto / fork 能力**（Lane 5 穷举确认）——它的 `agent-snapshot` 是「为测试的录制/回放」，不是历史分支。所以 `goto` 必须**自研**。可借 tarko 快照「按 loop 分目录存 + 校验」做底座（见 [05](./05-server-snapshot-context.md)）：截断 history 到目标 turn → 清理后续状态 → 从该 turn 开新分支（可选存为分支快照）。VRover 的 turn 边界天然落在「带 image 的 observe 消息」上（`context.ts#groupTurns`），`goto` 的切片点可用它。
