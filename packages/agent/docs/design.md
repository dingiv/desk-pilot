# Agent 模块设计：单脑 → 多脑（思路）

> 状态：**构想 / 方向性设计**。已落地部分以代码为准（类型见 `src/types.ts`，实现见 `src/core.ts`）；本文标 🔼/🆕 的是**提议**，尚未实现，标 ✅ 的为**已实现**。总览见 [index.md](./index.md)。
>
> **执行层已落地（✅）**：team loop / `tick` / 写锁 / `DeliverTask`（含 fan-out）/ resource+lease 已实现于 `src/team.ts` + `src/resources.ts`（外加 `types.ts`/`core.ts` 的增量接缝）。运行机制与类型草案见 [`execution-model.md`](./execution-model.md)——本文 §5.1/§5.3/§5.4/§5.5/§5.6/§3 已据此回改；凡涉及「team loop 怎么跑」的细节以 execution-model.md 为准。
>
> 本文回答：**为什么多脑**、**目标模型长什么样**、**怎么从现状演化**，并用两个具体场景（DeepSeek 写文章 / GLM + GUI-TARS 操作 GUI）验证设计。
>
> **代码风格**（见本包 `CLAUDE.md`）：面向数据的作用域与生命周期——核心状态用**纯数据接口**表达，行为用**组合多个纯动作接口**表达；子类型用 **interface extends**（接口继承，非类继承）；依赖走接口、实现藏在工厂函数后（`createX(deps)` / `destroyX(ins)`）。本文所有类型草案遵循此风格，取代此前 `index.md` 里的零散草稿。

---

## 1. 现状速写（单脑）

今天的 `@vrover/agent` 是**一个脑**：

- `Agent`（`core.ts:120`）= 共享协作者的持有者 + `Task` 工厂。**本身无会话状态**，可驱动多个独立 `Task`。
- `Brain`（`core.ts:50`，内部接口）= 共享协作者束：`platform`、`complete`、`runTool`、`nativeParser`、`tools`、`system`、`log`、配置旋钮、`memory`。
- `Task`（`core.ts:151`）= 一次会话的生命周期：`history`/`steps`/`status` + `run`/`exec`/`goto`/`pause`/`step`/`save`，并流式发事件。
- `CompleteFn`（`@vrover/llm`）= 大脑出口，`(req) => Promise<LLMResponse>`，loop 依赖注入，测试用假 LLM 零成本跑。
- **`step.ts` 已经把 `observe()` / `act()` 拆成纯动作件**——这是后续「组合动作接口」重构的种子；但 `Task.exec`（`core.ts:253`）把 `observe(capture+SoM) → complete(mark tools) → act(mark→coord)` **写死**了。
- `pruneForModel`（`context.ts:29`）= 纯函数，从全量 history 派生发给模型的裁剪后消息。
- `PromptRegistry`（`prompts/`）= 模板注册。
- `MemoryManager` / `FileMemoryManager` = per-Task 快照持久化。

它已经满足「一个脑驱动多个独立会话」——这正是多脑架构需要的形状：把「脑」从单数变成可编排的复数。

## 2. 驱动力（详）

**(a) 角色专业化 = 模型能力路由。** 不同模型各有强项：画图、编码、多模态理解、本地低延迟。让每个 Agent 绑定**最匹配其专长的模型**，子任务路由到对的 Agent，而不是逼一个通用模型包揽。Agent 的「自我介绍 / 能力宣告」（`profile`）就是路由依据。

**(b) 并发，而非串行等待。** 「一个 Agent 同时只跑一个 Task」是**角色层**约束（不分心）；但 `AgentTeam` 同时持有多个 Agent，可并发跑多个 Task，把「等单一模型推理」并行掉。

**(c) GUI agent 的双 VLM 典范。** 一个**规划 VLM**（大、负责推理「任务怎么完成」）+ 一个**操作 VLM**（小、专做 GUI 操作、可本地部署、快）。这天然映射为 `LeaderAgent`（规划）+ `GUIAgent`（操作）。

> 三者合一：**专长（用对的模型）+ 并发（不互相等）+ 双 VLM（规划 / 操作分工）**。

## 3. 场景验证（用两个场景压测设计）

### 场景一：DeepSeek 4.1，写文章（无 GUI）

```
Model 池: { deepseek: createChatModel({ id:'deepseek-4.1', ... }) }   // 一个 Model，两个 Agent 共用
Team     = createAgentTeam({ leader, workers:[writer], memoryStore })  // 无 desktop 资源

用户 Task("写一篇关于 X 的文章")
 └ leader.run(task)
     leader 组合 deepseek.complete → plan("写文章")
       → Plan{ steps:[ "写引言","写正文·三点","写结尾","润色" ] }
 └ Team 把 Plan 展开成 worker 子任务，分派给 writer（单 worker → 串行）
 └ writer.run(subtask)
     writer.exec(): deepseek.complete(messages) → 文本          // 无 observe / 无 act / 无 desktop
 └ 汇总 → done
```
**关键：无 desktop、无 SoM、无 observe/act；Leader 与 Worker 共用同一个 Model。差异纯在角色（提示词 / 行为），不在模型。**

### 场景二：GLM 4.6V 规划 + 本地 GUI-TARS 操作

```
Model 池: { glm:  createChatModel({ id:'glm-4.6v', modalities:['text','image'] }),
            tars: createGroundingModel({ id:'gui-tars', ... }) }       // 两个不同 Model
资源池:   { scout: desktopResource({ id:'visual_scout', exclusive:true }) }  // 独占，接管一个桌面
Team     = createAgentTeam({ leader(glm), workers:[operator(tars)], resources:[scout], ... })

用户 Task("登录某网站并导出账单")
 └ team.run(task)        // team loop 推进 leader 的 Task
     leader.tick: glm.complete(读 operator 回传的截图) → deliver_task(operator, "点登录按钮")
       // leader 不自己截图——视觉只归 operator（GUIAgent）；多模态消费的是 operator 回传的图
 └ operator 先经 lease 持有 desktop 工具面（Platform 在工具内部，不外露，§5.5）
 └ operator.tick:
     operator 组合 Observes + Grounds + Acts（经 desktop 工具面）：
       observe(): desktop.captureScreen() → Observation{png}          // 无 SoM、无 getElements
       ground():  tars.ground(png,"点登录按钮") → click(x,y)         // 模型直接出像素
       act():     desktop.performClick(x,y)                           // 像素直达，不经 mark→coord
 └ 子任务完成 → 结果作为 deliver_task 的 tool_result 回传 leader → release lease
```
**关键：operator 用像素坐标工具（非 mark）；grounding 由模型（GUI-TARS）完成，绕过 SoM；Leader 多模态、只规划不操作、不自己截图（图由 operator 回传）；desktop 经 lease 独占持有。**

## 4. 场景揭示的设计压力 → 核心洞察

**P1 — 「observe→think→act 的 SoM 循环」只是一种执行策略，不是 step 的通用形状。**
写文章没有 observe/act；GUI-TARS 的 observe 只截图、模型直接出像素。现在 `Task.exec` 把 SoM 循环写死了——它只是众多执行策略里的一种（「SoM mark 型 GUI 步」）。

**P2 — desktop/Platform 与 mark 工具是可选协作者，不是必备件。** 场景一无 desktop；场景二有 desktop 但工具是像素型、不经 `@vrover/tools` 的 mark 派发。

**P3 — Model 可共享（场景一）也可各异（场景二）；而且不是所有模型都是 `Completes`。** GUI-TARS 的接口是 `ground(截图,提示)→像素动作`，不是 `complete(messages,tools)`。**能力应在「动作接口」层抽象，Model 只是「一个端点连接 + 它提供的若干能力接口」。**

**P4 — Leader 的产物是结构化 `Plan`（数据），Team 把它物化成 worker 子 Task。** 存在两层粒度：规划 Task vs 执行 Task；执行 Task 内部还可能是循环。

> **核心洞察（落到风格）**：把行为拆成**可组合的纯动作接口**（`Observes` / `Plans` / `Acts` / `Grounds` / `Completes`），**Agent 的差异 = 组合了哪些接口**；worker 按能力命名、用**子接口继承** Agent（`LeaderAgent` / `GUIAgent` / `PaintAgent`）。现有 `step.ts` 里的 `observe()` / `act()` 已是拆好的件——只需新增 `ground()` 作为 `complete()+mark 派发` 的替代路径，再让每个 Agent 按需组合，多种 worker 就自然落出，且不破坏现有循环。
>
> **Team 是多类型资源池**：多 Agent、多 Model、多 Tool（web-search / local-rag / bash / skills / visual_scout …）、多 Task + 单例全局记忆库。资源分**共享**与**独占**两种访问语义——独占的 visual_scout 接管一个桌面，一时刻仅一个 agent 持有。这把「多 agent 抢同一桌面」从并发难题变成**资源租赁语义**。

## 5. 目标概念模型

### 5.1 纯动作接口（行为，可组合）

```ts
/** 观察：产出当前世界的 Observation（GUI=截图(+可选 SoM)；纯文本=空）。✅ 仅 GUIAgent 组合——视觉截图接口的唯一具体感知者。 */
interface Observes { observe(): Promise<Observation> }
/** 规划：✅ 已溶解——leader 不再一次性吐 Plan，而是 complete() + DeliverTask 边想边派（见 execution-model.md §2/§3）。 */
interface Plans    { plan(goal: string, obs?: Observation): Promise<Plan> }
/** 执行：经 desktop 工具面落一个动作；Platform 是工具内部资源，agent 不直接持有（§5.5）。 */
interface Acts     { act(action: PlatformAction): Promise<ActionResult> }
/** 像素级 grounding：截图 + 操作提示 → 像素动作（GUI-TARS 类模型）。 */
interface Grounds  { ground(obs: Observation, hint: string): Promise<PlatformAction> }
/** 聊天补全：messages(+tools) → 文本 / tool_calls（DeepSeek / GLM 类模型）。 */
interface Completes{ complete(req: CompleteRequest): Promise<LLMResponse> }
```

支撑数据类型（纯数据）：

```ts
interface Observation { png?: Buffer; somTable?: SoMElement[] }   // somTable 仅 SoM-mark 路径
interface Plan        { goal: string; steps: PlanStep[] }
interface PlanStep    { id: string; description: string }          // 给 worker 的高层提示
type PlatformAction =
  | { kind: 'click';   x: number; y: number }
  | { kind: 'type';    text: string; x?: number; y?: number }
  | { kind: 'scroll';  x: number; y: number; dir: 'up' | 'down' }
  | { kind: 'keypress'; keys: string[] }
  | { kind: 'done';    summary?: string }
interface ActionResult { ok: boolean; message: string }
```

> `Acts` 统一吃像素级 `PlatformAction`；mark→像素的解析是 **SoM-mark 型 GUIAgent 组合胶水**里的一步（即现 `@vrover/tools` 的 `dispatch`），不在 `Acts` 里。无 desktop 的 agent（如 PaintAgent）不组合 `Acts`。

### 5.2 Model — 连接 + 能力接口（并非都是 Completes）🔼

```ts
interface Model {
  readonly id: string
  readonly contextWindow: number      // 驱动 ContextManager 的窗口策略
  readonly modalities: Modality[]     // 'text' | 'image' | ...
}
// DeepSeek / GLM  → createChatModel(deps)      : Model & Completes
// GUI-TARS        → createGroundingModel(deps) : Model & Grounds
// 图像生成        → createImageModel(deps)     : Model & Generates   （PaintAgent 用）
```

Model 由 **Team 持有、Agent 引用**：可共享（场景一：leader 与 writer 共用 `deepseek`）也可各异（场景二：`glm` / `tars`）。一个 Agent 绑主模型（匹配专长），可选附加模型用于路由 / 兜底。

### 5.3 Agent — 数据 + per-Agent PromptBuilder + 子接口继承

```ts
interface AgentProfile {                 // 纯数据：能力宣告 + 路由依据 ✅
  id: string
  role: 'leader' | 'worker'              // ✅
  specialties: Specialty[]               // 'planning'|'gui-operate'|'gui-ground'|'paint'|'coding'|...
  models: Model[]                        // 绑定的模型（v1 暂直用 CompleteFn，Model 对象后续落地）
  bio: string                            // 一句话自述，**只注入 leader 的 PromptBuilder**（worker 互相不可见）
  requires?: string[]                    // 需要独占持有的资源 id（如 desktop）— ✅ 已实现（§5.5）
}
interface TeamAgent {                    // ✅ 已实现（src/team.ts）
  readonly profile: AgentProfile
  createTask(goal, opts?): Task          // mint 一个本 agent 拥有的 Task（ownerId 绑定）
  tick(task): Promise<TickOutcome>       // 推进一格——tick 形状 = 该 Agent 组合的行为（per-agent）
}

// leader 是**一族实现**（领域不同 → PromptBuilder / 模型 / 路由的 worker 不同）；一个团队**仅激活一个** leader ✅
interface LeaderAgent   extends TeamAgent {}           // complete + DeliverTask 委派；不 observe（Plans 已溶解）
interface GUILeaderAgent extends LeaderAgent {}        // 多模态：读 worker 回传的截图来规划（不自己 capture）
interface CodeLeaderAgent extends LeaderAgent {}       // 纯文本/代码：无视觉
interface GUIAgent    extends TeamAgent {}             // worker：视觉唯一感知者（observe/act 经 desktop 工具面）；持有 desktop lease
interface PaintAgent  extends TeamAgent {}             // worker：生成图片（绑图像模型）
// ……按专长扩展：CoderAgent / ResearchAgent / ...
```

- 每个 Agent 一个 **PromptBuilder**：它擅长的任务不同 → 提示形状不同 + 相关 Tools 不同。
- **Task 持有会话状态 + 生命周期**；team loop 下，**一次 `tick` 做什么委托给所绑 Agent 的组合行为** ✅——`observe→complete→act` 不再写死，而是 GUIAgent 的 tick（P1 落地点；GUIAgent 的 tick 即旧 `exec()`，桥接复用，见 execution-model.md §2）。

### 5.4 AgentTeam — 多类型资源池 + 调度器（对外第一句柄 · 组合根 · Task 工厂）

```ts
interface AgentTeam {                        // ✅ 已实现（src/team.ts；本块是目标全貌，v1 先落地标注项）
  // 多类型资源池
  readonly agents: ReadonlyMap<string, TeamAgent>   // ✅
  readonly models: ReadonlyMap<string, Model>       // 🔼
  readonly tools:  ReadonlyMap<string, Resource>    // web-search / local-rag / bash / skills / MCP / visual_scout …（§5.5）
  readonly tasks:  ReadonlyMap<string, Task>        // ✅
  readonly memoryStore: MemoryStore                 // 🔼 单例：全局记忆库
  readonly resources: ResourceManager               // ✅ 能力查询 + 独占租赁（§5.5）
  readonly roster: TeamRoster                       // ✅ worker 花名册（只喂给 leader）
  readonly leaderId: string                         // ✅ 单 leader
  // Task 工厂 + 调度
  createTask(goal, opts?): Task                     // ✅ ownerId 默认 leader
  run(task, opts?): Promise<TaskResult>             // ✅ team loop 跑到终态（取代旧 dispatch(plan)）
}
```

> **派发模型已变（见 [`execution-model.md`](./execution-model.md)）**：原 `Team.dispatch(plan)`（Team 把 leader 的 `Plan` 物化成子任务）**已废弃**——派发收归 leader 自己：它在 `tick` 里调 `deliver_task` 把子任务投给 worker、挂起自己；team loop 在子任务终态后机械完成、重载 leader。`AgentTeam.run(task)` 即「在 team loop 下把（leader 的）根 Task 跑到终态」。

Team 把目标交给 Leader，leader 经 `deliver_task` 委派；team loop 按「专长 + 资源可用性」（独占资源是否空闲）调度可推进的 Task。

### 5.5 Resources & Tools — 共享 vs 独占租赁；Platform 是工具的内部资源 ✅

池里每个资源带**能力描述**（PromptBuilder 选择依据）和**访问语义**：

```ts
type ResourceKind = 'service' | 'desktop' | 'image' | 'mcp' | 'skill' | ...
interface Resource {                       // ✅
  readonly id: string
  readonly capability: string              // 人/模型可读：做什么
  readonly exclusive: boolean              // 独占？
  readonly kind: ResourceKind
}
interface Lease { readonly resource: string; readonly holder: string /* agentId */ }   // ✅

interface ResourceManager {                // ✅ src/resources.ts
  acquire(resId: string, holder: string): Lease | null     // 独占：拿到 / 被占返回 null
  release(lease: Lease): void
  holder(resId: string): string | undefined
  select(need: string): Resource[]                         // 🔼 PromptBuilder 按能力挑相关资源
}
```

- **共享 service tools**（`exclusive:false`）：web-search、local-rag、bash、**MCP server**、**skills** 管理器——可并发（可能限流），任何 agent 经 tool-call 调用。
- **独占 desktop 资源**（`exclusive:true`）：接管一个桌面（本地或远程），**一时刻仅一个 agent 持有**。`acquire` 返回的是一个 **`Lease`（令牌）**，**不是 Platform**——持有者的 Task 因此被允许推进；team loop 保证独占性（非持有者的 Task 本 round 跳过）。
- **Platform 是工具的内部资源** ✅：desktop 资源在内部拥有 `Platform`（PipeWire capture / uinput input），并把它包装成一个 **`DesktopTool`**（实现 `Platform` 接口）注入到 GUIAgent 的核心 Agent；agent 永远只见 `DesktopTool`（工具面），**不见原始 Platform**——正如 web-search 工具内部持有 HTTP client。`acquire`/`release` 管的是「谁的 Task 可以推进」，Platform 本身始终在工具内部、不外露。
- **visual_scout 不再有「双重身份」问题**（原 §10 已决）：它就是一个 desktop 资源（`DesktopTool`），Platform 是其内部资源。`kind:'desktop'` 仅说明它经 lease 独占、且其工具面是 `Platform` 形状。
- **配置**：每个内置 tool 的属性（启用 / 参数 / 连接信息）由配置文件（`vrover.conf`）管理；`createAgentTeam` 读配置实例化它们。用户可在配置里挂**自己的 MCP server 与 skills**，它们成为池中的共享工具，由 PromptBuilder 注入相关 agent 的提示（§5.6）。
- 独占租赁把原 §7「Platform 并发竞争」从难题变成**资源语义**：桌面天然独占，调度器保证唯一持有者的 Task 在跑。

### 5.6 PromptBuilder — per-Agent，Team-aware 🔼

```ts
interface PromptBuilder {
  build(ctx: PromptCtx): PromptParts   // system 模板 + 相关 Tools schema + 记忆 + profile + Task 状态
}
```

- 每个 Agent 一个，按 agent 专长构造提示形状。
- 它**查询 Team 资源池**（`resources.select(need)`），把**相关的 service tools** schema 注入提示——工具来源 = **内置 tools（属性由 `vrover.conf` 管）+ 用户配置的 MCP server + skills**（动态工具注入，对齐项目 [decisions.md](../../docs/decisions.md) D8）。v1 leader 的 PromptBuilder 是 `renderLeaderSystem(roster)` 的简化版（注入 worker 花名册 + `deliver_task`/`finish`），完整 PromptBuilder 后续落地。
- 对 **desktop 资源**：仅当本 agent 持有 lease 时，才注入其 GUI 工具面。
- **信息流**：worker 的 `bio`/`specialties` 只进 **leader** 的 PromptBuilder（worker 互相不可见 → 单 leader 树、无横向委派）。
- 取代现 `PromptRegistry`（模板部分保留为 PromptBuilder 的一个数据源）。

### 5.7 Engine / ContextManager / 记忆

- **Engine**（原内部 `Brain`，显式化）：共享协作者束，由 Team 持有并注入给 Agent。
- **ContextManager**（per-Task，1:1）🔼：把 `pruneForModel` 提升为有状态对象；按**当前 Agent 的 Model** 调窗口策略（多模型上下文上限不同）；注入来自 `MemoryStore` 的跨 Task 记忆。
- **MemoryStore**（Team 级单例）🆕：全局记忆库 + 文件管理，跨 Task / 跨 Agent 共享。
- **MemoryManager**（Task 级）✅：per-Task 快照持久化（现有 `FileMemoryManager`）。

### 5.8 所有权与生命周期

```
AgentTeam  ──拥有──▶ { Agents · Models · Resources(Tools) · Tasks · MemoryStore(单例) }
Task       ──拥有──▶ ContextManager   (1:1)
Agent      ──拥有──▶ PromptBuilder    (1:1)
exclusive Resource (visual_scout) ──租赁给──▶ ≤1 Agent（一时刻）
```

```ts
function createAgentTeam(deps): AgentTeam
function destroyAgentTeam(team: AgentTeam): void   // 释放所有 lease、关闭各 Model 连接、回收
function createLeaderAgent(deps): LeaderAgent
function createGUIAgent(deps): GUIAgent
function createPaintAgent(deps): PaintAgent
```

## 6. 角色专业化与路由

路由 = 读各 Agent 的 `profile.bio / specialties`，把（子）任务派给最匹配的 Agent。Leader 做这件事；无 Leader 的退化情形由 Team 直接派。**双 VLM 是典范配置**：planner 出步骤，GUIAgent 执行，遇阻回 planner——Task 在两者间接力。

**Team 调度同时考虑资源可用性**：需要 desktop 的子任务只能派给能 `acquire` 到 `visual_scout` lease 的 GUIAgent；独占资源被占时，子任务排队或改派到空闲 agent。

> 注意场景一的提醒：专业化**可以纯在角色**（同模型、不同提示 / 行为），不必每次都换模型。路由依据是 `profile`，模型只是 profile 的一项。

## 7. 并发模型（资源语义）

- 单 Agent 单 Task（角色层不并发）。
- Team 多 Agent 多 Task 并发 → 把「等单一模型推理」并行掉。
- **并发空间由资源语义界定**：共享 service tools 可并发；独占 desktop 资源由 lease 保证唯一持有者。真正的并行发生在「不同 Model 推理」「不同 desktop」「纯 think 阶段」。
- ⚠️ 仍需注意：**共享 service tools 的限流**（web-search / bash 的并发上限）、**Model 并发请求**的连接池。开放问题见 §10。

## 8. 与现有代码的映射 + 演化路径（不重写）

多脑是单脑的泛化。逐步提升现有最小缝，每步保持测试全绿：

1. 抽出 `Grounds` 动作接口 + `createGroundingModel`（GUI-TARS）——SoM 之外的另一条 grounding 路径。
2. **重构 `Task.exec`，把 step 形状委托给所绑 Agent**（P1 解锁）——`observe→complete→act` 降级为某类 GUIAgent 的行为。
3. `CompleteFn` → `Model & Completes`（chat 模型）；`createChatModel`。
4. `pruneForModel` → `ContextManager`（per-Task）；`PromptRegistry` → `PromptBuilder`（per-Agent）。
5. 内部 `Brain` → 显式 `Engine`；加 `AgentProfile`；现 `AgentImpl` 等价于「单 Agent 的 Team」。
6. 加 `LeaderAgent`（`Plans`）+ `Plan` + `Team.dispatch`；worker 子接口（`GUIAgent`/`PaintAgent`/...）。
7. **资源池 + `ResourceManager`**：把 tools / models / desktop 收进池；`visual_scout` 作为独占 desktop 资源 + Platform 来源；acquire/release 租赁。
8. Team 级并发调度（按专长 + 资源可用性）。

> **落地进度（✅ 已做）**：team loop + `tick` + 写锁 + `DeliverTask`（含 fan-out）覆盖了上面的 2、6、8 的执行层（见 [`execution-model.md`](./execution-model.md) 与 `src/team.ts`）；`ResourceManager` + `Lease` + `DesktopTool`（独占 desktop、Platform 内藏于工具）覆盖了 7 的资源层（`src/resources.ts`）。**未做**：1（`Grounds`/GUI-TARS）、3（`Model` 对象，v1 直用 `CompleteFn`）、4（`ContextManager`/完整 `PromptBuilder`）、5（`Brain`→`Engine` 显式化）、`MemoryStore` 全局层、MCP/skills 的真实接入与 `vrover.conf` 工具属性。
>
> 退化等价：`createAgent(opts).run(goal)`（今天）≡ 单 Agent Team、无 Leader、无资源池的 `team.run(task)`（v1 已具备此形状）。

## 9. 设计原则（延续项目既有 + 本包风格）

- **面向数据的作用域与生命周期**：核心状态 = 纯数据接口（`AgentProfile`/`Plan`/`Observation`/`Resource`/Task 状态）；行为 = 组合纯动作接口；子类型用 interface extends；实现藏在 `createX` 后、`destroyX` 收尾。无 IOC 容器即达 SOLID。
- **组合优于继承（类层面）**：Agent / Team 都是组装协作者与动作接口；接口继承（`GUIAgent extends Agent`）只声明能力组合，不引入实现基类。
- **纯核脏壳**：构造无副作用；密钥 / 连接在 `Model`，文件在 `MemoryStore`，OS / 桌面在 desktop 资源的 Platform；核心（ContextManager 裁剪、PromptBuilder 组装、ResourceManager 租赁、Task 状态机、纯动作接口）可注入假模型 / 假资源零成本单测。
- **「一个脑驱动多会话」性质保住**：只是脑从单数变可编排的复数。

## 10. 未决问题（🟡）

- **能力宣告 / 资源能力描述格式**：`specialties` 固定枚举 vs 自由标签？`Resource.capability` 怎么写让 PromptBuilder 与 Leader 模型都能读？
- **路由 + 调度策略**：能力匹配规则 vs 让 Leader 模型决定？独占资源被占时排队 vs 改派？
- **独占租赁粒度**：lease 超时 / 抢占 / 排队顺序？崩溃后回收？
- **`Plan` 结构**：✅ 已溶解——leader 不再产出完整 `Plan`，而是 `complete()` + `deliver_task` 边想边派（fan-out 已支持，wait-for-all）。DAG 形态留待需要时再加。
- **step 策略的抽象边界**：委托给 Agent 的「step 形状」用哪种抽象（一个 `Stepper` 接口？还是直接组合 `Observes/Completes/Acts` 由 Agent 自行编排）？
- **`Grounds` vs `Completes` 模型边界**：一个模型同时支持两种能力时如何暴露？
- **像素 vs mark 工具面**：SoM-mark 型 GUIAgent 的 mark→像素胶水（现 `@vrover/tools` dispatch）归 Worker 组合，还是保留为可复用件？
- **visual_scout 双重身份**：✅ 已决——它就是一个 desktop 资源（`DesktopTool`），Platform 是其内部资源；`acquire` 返回 `Lease` 令牌（不返回 Platform），持有者的 Task 被允许推进。原「资源 vs Platform 来源」的分界问题消失。
- **PromptBuilder 的相关性判定**：用什么策略从池里挑「相关」tools 注入（关键词 / embedding / 让模型自选）？
- **Agent 间接力**：Task 在 Agent 间传递时，ContextManager 如何跟着 Task 走、窗口策略如何平滑切换。
- **共享资源限流 / Model 并发**：web-search / bash 的并发上限、同一 Model 多请求的连接池。

## 11. 与 graph walker 的关系（正交）

多 Agent 自洽于本模块，**不依赖** walker。walker 在 `@vrover/scout` 按 M1 推进。未来复合方式（二选一或并存）：

- walker 的「当前 node + 高层操作」作为**数据源**注入 `PromptBuilder`；
- 或一个 GUIAgent 专门**持有 walker**（走已知图），另一个走视觉兜底——把「图优先 / 视觉兜底」也变成角色分工。

> 注：本模块里的 `visual_scout` 资源 = 持有一个桌面会话的 server；它与 `@vrover/scout` server 是同一个进程概念（见 [`../../docs/scout-server.md`](../../docs/scout-server.md)），这里强调的是它作为**独占资源**被 Team 调度的角色。
