# Agent 执行模型（team loop · 写锁 · DeliverTask）— 类型草案

> 状态：**构想 / 方向性，未实现**。代码以 `src/core.ts` / `src/types.ts` 现状为准；本文标 🔼 的为**提议**。
> 本文只给**执行层**（运行时机制）的类型草案；概念与动机见 [`design.md`](./design.md)（§5 概念模型 / §6 路由 / §7 并发）。类型定稿后，design.md 里被取代的段落（§5.1 `Plans`、§5.4 `dispatch`、§6 路由、§7 并发约束）会改成「见本文」。
>
> 风格延续本包 `CLAUDE.md`：核心状态 = 纯数据接口；行为 = 组合纯动作接口；子类型用 `interface extends`；实现藏在 `createX(deps)` 后、`destroyX(ins)` 收尾。

## 0. 速览

```
每个调度 round，TeamLoop：
  1. 机械完成扫描：有 worker 子 Task 终态？→ 把结果作为 tool_result 喂回挂起的 leader，唤醒它
  2. leader 优先：若 leader 的 Task runnable，tick 它一格
  3. worker 并发：其余 runnable 的 worker Task 各 tick 一格（不同 Task 不争同一写锁）
  4. 重复，直到根 Task 终态
写锁不变量：一个 Task 一时刻最多一个 in-flight tick（调度器不双重派发，无需 mutex）。
```

两个关键迁移（相对 `core.ts` 现状）：
- **行为离开 Task**：今天的 `Task.exec()`（`core.ts:253`，observe→complete→act 写死）上移为 **`Agent.tick(task)`**，且 **per-agent**（GUI worker 的 tick ≈ 今天的 `exec()`；leader 的 tick 是新的）。
- **重复离开 Task**：今天的 `Task.run()` while（`core.ts:209`）被 **`TeamLoop`** 接管；`run()` 退化为单 agent / 测试便捷路径。

---

## 1. Task — 状态持有者（行为上移到 Agent / TeamLoop）🔼

Task 只持有**一会话的状态 + 生命周期（持久化 / 回放 / 流式）**；loop 逻辑（`run`/`exec`/`pause`/`step`）迁出。`goto`（回放）与 `on/off`（流式）保留。

```ts
/** Task 生命周期。比现状（types.ts:46）新增 'suspended'。 */
export type AgentStatus = 'idle' | 'running' | 'suspended' | 'paused' | 'done' | 'error';

/**
 * 挂起等子任务：当 status === 'suspended' 时非空。team loop 用它做两件事——
 * (a) 判断 runnable=false，跳过本 Task；(b) 子 Task 终态时，按 toolUseId 把结果喂回 leader。
 */
export interface TaskSuspendState {
  readonly toolUseId: string;   // leader 这次 DeliverTask 的 tool_use id（喂回 tool_result 的键）
  readonly subtaskId: string;   // 它派生的 worker 子 Task
  readonly workerId: string;    // 接收子任务的 worker agent
}

export interface Task {
  readonly id: string;
  readonly goal: string;
  readonly ownerId: string;                 // 拥有此 Task 的 agent（写者绑定）
  readonly status: AgentStatus;
  readonly history: readonly Message[];     // 全量会话；裁剪只在发送时做
  readonly steps: readonly AgentStep[];     // GUI worker 有 elements；leader 的 step 形状不同（见 §2）
  readonly result?: TaskResult;
  readonly suspendedOn?: TaskSuspendState;  // status === 'suspended' 时非空

  /** 纯状态派生：status 允许 team loop 推进（不含并发 in-flight 判断，那由调度器持有）。 */
  readonly runnable: boolean;               // status ∈ {idle, running} && !suspendedOn

  // ── 生命周期（保留）──
  goto(step: number): void;                 // 破坏性回放到第 N 步
  save(): Promise<void>;                    // 经 MemoryManager 持久化快照
  on(listener: TaskListener): void;
  off(listener: TaskListener): void;
}
```

> `run` / `exec` / `pause` / `step` 不再在 Task 上：
> - `exec` → `Agent.tick(task)`（§2）；
> - `run`（重复）→ `TeamLoop.run(rootTaskId)`（§4）；
> - `pause` → `TeamLoop.stop()` 或「不再 tick 该 Task」；
> - `step`（单步调试）→ `TeamLoop.round()` 一次。
>
> 单 agent 退化路径（design.md §8）：`createAgent(opts).run(goal)` ≡ 单 agent、无 team loop 的 `round()` 循环。

---

## 2. Agent — 行为持有者；tick 是 per-agent 的 🔼

team loop 对每个 agent 只调一个统一入口 `tick(task)`。**tick 的形状 = 该 Agent 组合了哪些动作接口**（§design.md §5.3「exec 委托给 Agent」的具体化）。

```ts
/** 一次 tick 的结果（纯数据）。team loop 据此决定下一 round 如何调度该 Task。 */
export type TickOutcome =
  | { kind: 'progress'; step?: AgentStep }      // 推进了一格，仍在跑（GUI worker 提交了一个 step）
  | { kind: 'suspended'; pending: TaskSuspendState }  // 撞上 DeliverTask，挂起（仅 leader 会产生）
  | { kind: 'done'; result: TaskResult }        // 终态：success / max_steps
  | { kind: 'error'; result: TaskResult };      // 终态：error

export interface Agent {
  readonly profile: AgentProfile;             // §6
  readonly promptBuilder: PromptBuilder;      // per-Agent，Team-aware（design.md §5.6）
  /** 推进 `task` 一格。读/写 task 的状态；协作者（model/tools/platform）来自本 Agent。 */
  tick(task: Task): Promise<TickOutcome>;
}

// ── per-agent 子类型（interface extends，非类继承）──
/**
 * leader：complete → 处理 tool_uses（普通 tool inline，遇 DeliverTask 则挂起）。规划 + 委派。
 * 不组合 Observes——视觉截图接口只归 GUIAgent（见下「视觉边界」）。leader 若需视觉信息，
 * 由 GUIAgent 经 deliver_task 结果回传（作为数据），而非自己 capture。
 *
 * leader 是**一族实现**：领域不同 → PromptBuilder / 模型 / 路由的 worker 不同。
 * 一个团队**仅激活一个** leader（见 §6）。
 */
export interface LeaderAgent extends Agent {}

/** GUI 任务 leader：多模态模型，消费 GUIAgent 回传的截图规划下一步；路由到 GUI worker。不自己 capture。 */
export interface GUILeaderAgent extends LeaderAgent {}

/** 编码任务 leader：纯文本/代码模型，无视觉；路由到 coder worker。 */
export interface CodeLeaderAgent extends LeaderAgent {}

/** GUI worker：tick = observe → complete → act（≈ 今天的 exec）；不会 suspended。视觉的唯一具体感知者。 */
export interface GUIAgent extends Agent, Observes, Acts {}

/** 图像 worker：tick = 生成；绑图像模型。 */
export interface PaintAgent extends Agent {}
```

各 tick 形状（实现藏在工厂后，这里只描行为）：

| Agent | tick 一格做什么 | 可能产出 |
|---|---|---|
| `GUIAgent` | `observe()` → `complete(mark tools)` → `act(mark→coord)`（即 `core.ts:253` 现状）；observe/act 经 **desktop 工具面**，Platform 在工具内部 | `progress` / `done` / `error` |
| `LeaderAgent` | `complete()` → 逐个处理 tool_uses；普通 tool inline 执行，遇 `DeliverTask` 则置 `suspendedOn` 并返回（**不 observe**） | `progress` / `suspended` / `done` / `error` |
| `PaintAgent` | 生成图片（经图像工具面） | `progress` / `done` |

> 动作接口 `Observes` / `Acts` / `Grounds` / `Completes` 及 `Plan`/`Observation`/`PlatformAction` 等支撑数据类型沿用 design.md §5.1，此处不重复。
> 注：leader 不再需要一个独立的 `Plans` 接口一次性吐 `Plan`——规划溶解进它的 `complete()` + `DeliverTask` 循环（见 §3）。`Plan` 可保留为内部表示，但不再是派发契约。
>
> **视觉边界**：`Observes`（截图 + SoM）只由 GUIAgent 组合——只有它「具体感知」视觉截图接口。leader / paint 等不 capture；leader 若要视觉，靠 GUIAgent 的 deliver_task 结果回传（图像/描述作为数据进 history）。
> **Platform 是工具的内部资源**：GUIAgent 的 `Acts`/`Observes` 不直接持有 `Platform`，而是调用 **desktop 工具的工具面**（capture / perform(action)）；`Platform`（PipeWire capture、uinput input）封装在该工具内部，与 web-search 工具内部持有 HTTP client 同构——agent 永远只见工具面，不见实现资源。

---

## 3. DeliverTask — leader 投递子任务的 tool 🔼

DeliverTask 是一个**工具**：leader 在 `complete()` 的 tool_uses 里调用它，把一个自己生成的子任务交给 worker，并**挂起自己的 Task**。team loop 在子任务终态后重载 leader（§4 机械完成）。

```ts
/** DeliverTask 的入参（tool input schema）。 */
export interface DeliverTaskInput {
  /** 目标 worker：agentId 直指，或给一个能力标签让 team/leader 路由解析。二选一的策略见 §9。 */
  readonly to: string;
  /** 交给 worker 的子任务目标（成为 worker 子 Task 的 goal）。 */
  readonly goal: string;
  /** 可选：leader 给的上下文 / 约束 / 此前产出，拼进 worker 的首条 user 消息。 */
  readonly context?: string;
}

/**
 * DeliverTask 的回填（mechanical completion 由 team loop 构造，作为 tool_result 喂回 leader）。
 * 即子 Task 的终态结果，投影成「leader 可读的一段文本 + 状态」。
 */
export interface DeliverTaskResult {
  readonly subtaskId: string;
  readonly status: TaskStatus;     // 复用 types.ts:35 的终态枚举（success / max_steps / error）
  readonly output: string;         // worker 的产出/结论（文本），进 leader 的 history
  readonly summary?: string;
}

/**
 * 一个 round 里，team loop 检测到某 worker 子 Task 终态、且其父 leader Task 正挂起于它时，
 * 构造此解析，把 DeliverTaskResult 包成 tool_result(tool_use_id) 追加进 leader 的 history，
 * 清 leader 的 suspendedOn、置回 runnable。
 */
export interface DelegateResolution {
  readonly parentTaskId: string;
  readonly toolUseId: string;
  readonly result: DeliverTaskResult;
}
```

作为 `@vrover/llm` `ToolDef` 的形状（示意）：

```ts
const DeliverTaskTool: ToolDef = {
  name: 'deliver_task',
  description: '把一个子任务交给指定 worker 执行，等待其完成结果后再继续。',
  input_schema: { /* to: string, goal: string, context?: string */ },
};
```

> leader 一个 tick 内若同时有 `deliver_task` 与普通 tool_call：先 inline 跑完普通 tool、再因 `deliver_task` 挂起（与现 `exec`「一轮跑完所有 tool_use」顺序一致）。一个 tick 假设至多一个待决 DeliverTask（多挂起的 DAG 形态留 §9）。

---

## 4. AgentTeam + TeamLoop — 资源池 + 调度器（协作 pump）🔼

`AgentTeam` = 多类型资源池 + Task 工厂（组合根）；它**组合**一个 `TeamLoop`（调度器）。team loop 是全新顶层循环，与 `core.ts` 里的 GUI worker loop 正交。

```ts
export interface AgentTeam {
  readonly loop: TeamLoop;                              // 调度器
  readonly agents: ReadonlyMap<string, Agent>;
  readonly models: ReadonlyMap<string, Model>;          // design.md §5.2
  readonly tools: ReadonlyMap<string, Resource>;        // 共享 service tools
  readonly resources: ResourceManager;                  // 共享/独占租赁（design.md §5.5）
  readonly tasks: ReadonlyMap<string, Task>;
  readonly memoryStore: MemoryStore;                    // 全局记忆库（design.md §5.7）

  // ── Task 工厂 ──
  createTask(goal: string, opts?: { id?: string; ownerId?: string }): Task;
  loadTask(id: string): Promise<Task | null>;
  /** 把一个（leader 的）根 Task 在 team loop 下跑到终态。即 design.md §5.4 `dispatch` 退化后的形态。 */
  run(task: Task): Promise<TaskResult>;
}

/** 协作 pump 调度器。每个 round 推所有 runnable Task 各一格（leader 优先，worker 可并发）。 */
export interface TeamLoop {
  /** 推进一个调度 round（见 §0 算法 + §8 数据流）。 */
  round(): Promise<void>;
  /** 自动跑到根 Task 终态。 */
  run(rootTaskId: string): Promise<TaskResult>;
  /** 协作停止：下一 round 边界不再派发。 */
  stop(): void;
}
```

`round()` 算法（伪码，实现藏在 `createTeamLoop` 后）：

```
async round():
  // (1) 机械完成扫描：把已终态的委派子 Task 结果喂回挂起的父 leader
  for res in resolveDelegatedSubtasks():           // 构造 DelegateResolution（§3）
    parent = tasks[res.parentTaskId]
    parent.history.append(tool_result(res.toolUseId, res.result))   // 同构于 core.ts:331-338
    parent.clearSuspend()                          // status → runnable
  // (2) leader 优先
  if leader.task.runnable && not inFlight(leader.task):
    inFlight.add(leader.task); fireAndForget(leader.tick(leader.task))   // 异步非阻塞
  // (3) 其余 runnable worker 并发各一格
  for task in runnableWorkerTasks() where not inFlight(task):
    inFlight.add(task); fireAndForget(owner(task).tick(task))
  // (4) 等本 round 所有 tick 落定，更新 status / inFlight / suspendedOn
  await allFired
```

### 4.1 工具/资源层（执行视角）

- **工具即资源**：built-in tools（web-search / bash / visual_scout …）、用户配置的 **MCP server**、**skills**，统一作为 AgentTeam 池里的 `Resource`，经 `ResourceManager` 管理（共享 vs 独占）。
- **内置工具可配置**：每个内置 tool 的属性（启用 / 参数 / 连接信息等）由配置文件（`vrover.conf`）管理；`createAgentTeam` 读配置实例化它们。
- **可扩展**：用户可在配置里挂自己的 MCP server 与 skills，它们成为池中工具，由 PromptBuilder 注入相关 agent 的提示（design.md §5.6）。
- **实现资源封装在工具内**：agent 永远只见「工具面」（schema + invoke），不见其内部资源——`Platform` 属 desktop 工具内部，HTTP client 属 web-search 内部，MCP 连接属 mcp 工具内部。
- **desktop 工具的独占性**：visual_scout 是 `exclusive` 资源，lease 保证一时刻仅一个 GUIAgent 持有其工具面；其内部的单个 Platform 自然只服务当前持有者。

> 资源 / 工具 / 配置 / MCP / skills 的完整规格属 design.md §5.5/§5.6 范畴；执行模型只关心：agent tick 时经工具面调用，工具内部资源（含 Platform）不外露。

---

## 5. 写锁与并发不变量（显式化）

用户模型：**Task 拥有写锁，一时刻仅一个 agent 处理**。落到 pump：

- **无需 mutex**。team loop 是唯一派发者；它保证**一个 Task 一时刻最多一个 in-flight tick**（`inFlight` 集合去重）。这是调度纪律，不是运行时锁原语。
- **1 Task : 1 owner agent : ≤1 in-flight tick**。Task 绑定单一写者（`ownerId`）；agent 一时刻也只 tick 一个 Task（角色层不并发——team loop 不对同一 agent 并发派两个 Task）。
- **挂起不「释放」任何锁**：挂起期间无人写 leader 的 Task（worker 只写自己的子 Task，是另一个 Task、另一把隐式锁）。这正是「写锁让 DeliverTask 挂起/重载不撕裂上下文」的落点——上下文永远只有一个写者。
- **真并发**发生在：不同 Model 推理、不同 desktop（不同 lease）、纯 think 阶段——即不同 Task 并行推进时（§0 第 3 步）。

> 诚实说明：写锁在这里不是一个 `AsyncMutex` 对象，而是 team loop 的调度不变量 + Task 的 `runnable`/`ownerId`。若未来允许多个调度器实例或跨进程，再升级成显式锁。

---

## 6. 能力宣告的信息流 🔼

```ts
export interface AgentProfile {
  readonly id: string;
  readonly role: 'leader' | 'worker';
  readonly specialties: readonly string[];   // 自由标签 or 枚举（design.md §10 未决）
  readonly models: readonly Model[];
  /** 人/模型可读的能力宣告（纯文本）。仅注入 leader 的 PromptBuilder。 */
  readonly bio: string;
}

/** team 的能力花名册——只有 leader 的 PromptBuilder 能读到 workers 的 profile。 */
export interface TeamRoster {
  readonly workers: readonly AgentProfile[];   // workers 互相不可见 → 无横向委派（单 leader 树）
}
```

- **信息流规则**：worker 的 `bio`/`specialties` 只进 **leader** 的 system prompt（供其 `deliver_task` 路由）；worker 的 PromptBuilder 拿不到 roster。
- `DeliverTaskInput.to` 的解析依据就是这份 roster（agentId 直指 / 按 specialty 匹配，§9）。
- **团队仅激活一个 leader**：leader 是领域专用的（`GUILeaderAgent` / `CodeLeaderAgent` / …），一个 AgentTeam 里**恰好一个** leader + N 个 worker；team loop 优先推进它。leader 的 `specialties` 反映其领域，自然只把 `deliver_task` 路由到匹配领域的 worker（GUI leader → GUI worker，Code leader → coder）。

---

## 7. 生命周期（工厂 / 回收）

```ts
export function createAgentTeam(deps: AgentTeamDeps): AgentTeam;
export function destroyAgentTeam(team: AgentTeam): void;   // 释放所有 lease、关 Model 连接、回收
export function createTeamLoop(deps: TeamLoopDeps): TeamLoop;
export function destroyTeamLoop(loop: TeamLoop): void;

export function createGUILeaderAgent(deps: LeaderAgentDeps): GUILeaderAgent;
export function createCodeLeaderAgent(deps: LeaderAgentDeps): CodeLeaderAgent;
export function createGUIAgent(deps: GUIAgentDeps): GUIAgent;
export function createPaintAgent(deps: PaintAgentDeps): PaintAgent;
// 对应 destroyX(ins) 收尾
```

---

## 8. 一个 round 的数据流（把上面串起来）

以「leader 派一个 GUI 子任务」为例，跨多个 round：

```
round k   leader.tick: complete() → tool_uses 含 deliver_task(to:operator, goal)
          → assistant 消息（含该 tool_use）append 进 leader.history
          → leader.suspendedOn = { toolUseId, subtaskId, workerId } ; status='suspended'
          → tick 返回 { kind:'suspended', pending }
round k+1 (机械完成扫描：子 Task 未终态，跳过)
          leader.runnable=false → 跳过
          worker 子 Task runnable → operator.tick(子Task) = observe→complete→act（提交一个 step）
round k+2 … 重复 tick worker，直到子 Task 终态（done/error/max_steps）
round n   (机械完成扫描：子 Task 终态)
          → 构造 DelegateResolution{ parentTaskId: leader, toolUseId, result: DeliverTaskResult }
          → leader.history.append(tool_result(toolUseId, result.output))   // 同构 core.ts:331-338
          → leader.clearSuspend() ; status → runnable
round n+1 leader.tick: complete() 在含上述 tool_result 的 history 上续跑（leader 据此做语义验收：是否真达成 / 要否重派）
          …直到根 Task 终态
```

- **机械完成（team loop）**：子 Task 终态 → 喂 tool_result → 唤醒 leader。
- **语义验收（leader，in-band）**：重载后读 tool_result，自己判断子任务是否达成、要不要再 `deliver_task`。

---

## 9. 未决微问题（🟡）

- **`suspended` 的表达**：本文取「`AgentStatus` 加 `'suspended'` 值 + `suspendedOn` 详情字段」。备选：不加 status 值，用 `running + suspendedOn` 表达。前者更易在调度器 switch。
- **leader 视觉路径（已定）**：leader 永不自己 capture（`Observes` 只归 GUIAgent）。视觉需求由 **leader 实现**决定——`GUILeaderAgent` 多模态、消费 GUIAgent 经 `deliver_task` 回传的截图来规划；`CodeLeaderAgent` 无视觉。即「纯靠 worker 回传」，且是否需要视觉是 per-leader-impl 的。design.md §3 场景二「leader 带截图」需改写为「leader 经 worker 结果获图」。
- **visual_scout 双重身份（design.md §10）已消解**（备忘）：它就是一个 desktop 工具（资源），Platform 是其内部资源——不再是「资源 vs Platform 来源」的分界问题。
- **round 内并发 vs 串行**：§0/§4 取「worker 可并发各一格」（对齐 design.md §7 的真并发诉求）。若要强确定论，可退化为严格轮转（一 round 一 tick）。待定。
- **资源门控时机（已定）**：取**调度期门控**——team loop 在 tick 前为 worker Task `acquireRequired` 其 `profile.requires` 里的独占资源；被占则本轮跳过该 Task。lease **跨整个 Task 持有**（首 tick 获取、续 tick 复用、终态 `releaseTerminated` 释放），避免中途被别的 worker 抢占导致 GUI 状态交错。实现见 `src/resources.ts` + `team.ts` 的 `acquireRequired`。
- **`DeliverTaskInput.to` 的路由**：`agentId` 直指 vs 能力标签（`specialty`）让 team 解析。后者更接近「按能力路由」，但解析规则待定。
- **一个 leader Task 同时多个待决 DeliverTask（已实现）**：`suspendedOn` 已是集合；leader 一个 tick 可 fan-out N 个 `deliver_task`，挂起等**全部**完成（wait-for-all）再续跑——每个子任务结果各作为一个 `tool_result` 喂回。「任一完成即续跑」（wait-for-any）目前未做，留待需要时再加。
- **GUI worker 的 `AgentStep`（含 elements 计数）与 leader 的 step 形状不同**：leader 的 step 记录（无 elements、记的是「派了哪些子任务」）需单独定义，留 types 细化。
