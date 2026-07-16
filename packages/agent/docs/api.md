# 对外接口（Public API）

> `@vrover/agent` 暴露给上层（agent UI 界面模块、CLI、web server）的接口总览。上层通过 `AgentTeam` 创建/运行/观察任务、管理资源；不需要关心 team loop 内部调度与写锁。
>
> 概念模型见 [`design.md`](./design.md) 与 [`index.md`](./index.md)；运行时机制见 [`execution-model.md`](./execution-model.md)。本文只描述 **已可用的导出**（对应 `src/index.ts`）及 **UI 需要的、尚未提供的** 缺口。

---

## 概述

```
UI ──AgentTeam（组合根）──▶ TeamLoop（自动调度）
        ├─ agents: Map<id, TeamAgent>      // Leader + Worker(s)
        ├─ tasks:  Map<id, Task>           // 所有任务（根 + 子）
        ├─ resources: ResourceManager      // 工具池 + 独占租赁
        ├─ roster:  TeamRoster             // worker 花名册（只供 leader 看）
        └─ createTask / run                // 对外：创建根 Task、跑到终态
```

`AgentTeam` 是唯一的对外句柄。UI **不直接**操作 `TeamLoop` / `TeamAgent` / `ResourceManager` 的内部状态——读取可读，写入经 team。

---

## 1. 创建 AgentTeam（组合根）

```ts
import { createAgentTeam, createLeaderAgent, createGUIAgent, createGroundingAgent,
         createChatModel, createDesktopTool, createResourceManager } from '@vrover/agent';

const team = createAgentTeam({
  leader,                                      // LeaderAgent
  workers: [guiWorker, paintWorker, ...],      // TeamAgent[]
  resources: [desktopTool, webSearch, ...],    // Resource[]（可选）
});
```

### `AgentTeam` 对外属性与方法

| 成员 | 类型 | 说明 |
|---|---|---|
| `loop` | `TeamLoop` | 调度器。通常不直接调——经 `run()`；手动模式可 `loop.round()` |
| `agents` | `ReadonlyMap<string, TeamAgent>` | 所有 agent（key = `profile.id`） |
| `tasks` | `ReadonlyMap<string, Task>` | 所有任务（根 + 所有委派子任务） |
| `roster` | `TeamRoster` | `{ workers: AgentProfile[] }` — UI 可直接读 |
| `leaderId` | `string` | 当前活跃 leader 的 agent id |
| `resources` | `ResourceManager` | 资源池 + 租赁（可 `register` 新资源、读 `holder`） |
| `createTask(goal, {ownerId?})` | `Task` | 创建一个根 Task（默认 owner = leader） |
| `run(task, {maxRounds?})` | `Promise<TaskResult>` | 在 team loop 下跑到终态 |

### `TeamLoop`（通常不直接调）

| 成员 | 说明 |
|---|---|
| `round()` | 手动推进一个调度 round（单步调试） |
| `run(rootTaskId, {maxRounds?})` | 指定 taskId 跑到终态 |
| `stop()` | 协作停止，下一 round 边界不再派发 |

---

## 2. 创建 Agent

### 2.1 LeaderAgent

```ts
const leader = createLeaderAgent({
  profile: { id: 'leader-1', role: 'leader', specialties: ['gui-planning'],
             bio: '多模态规划者：看图分解 GUI 任务，分派给 GUI worker', models: [...] },
  complete: model.complete,          // CompleteFn（leader 调 planner VLM）
  roster: { workers: [guiProfile, paintProfile, ...] },  // 供 leader 的 deliver_task 路由
  systemPrompt: '...',              // 可选，覆盖默认 leader system prompt
  maxSteps: 100,                    // 可选，默认 100
});
```

- leader 的 `tick` = `complete()` + 处理 `deliver_task` / `finish`。它**不截图**，视觉来自 worker 回传。
- `profile.bio` 被注入 leader 的 system prompt（供模型路由到正确的 worker）。

### 2.2 GUIAgent（SoM-mark 型）

```ts
import { createAgent } from '@vrover/agent';

const core = createAgent({              // 现有的单脑 observe→think→act 循环
  platform, complete, tools, ...        // 标准 AgentDeps
});
const guiWorker = createGUIAgent({
  profile: { id: 'worker-gui', role: 'worker', specialties: ['gui-operate'],
             bio: 'GUI 操作者：截图 + SoM mark 工具 + 执行', requires: ['visual_scout'] },
  core,
});
```

- `requires: ['visual_scout']` 声明该 agent 需要桌面 lease。team loop 在 tick 前自动 `acquire`；终态后 `release`。
- tick = 一次 `core.createTask` 产的 Task 调 `exec()`（现有的 SoM 循环）。

### 2.3 GUIAgent（pixel-grounding 型，GUI-TARS 类）

```ts
const groundModel = createGroundingModel({
  id: 'gui-tars', contextWindow: 4096, modalities: ['image'],
  ground: (obs, hint) => { /* 调用 GUI-TARS 推理 */ },
});

const groundWorker = createGroundingAgent({
  profile: { id: 'worker-ground', role: 'worker', specialties: ['gui-ground'],
             bio: '像素级 grounding：截图 + 提示直接出像素坐标', requires: ['visual_scout'] },
  model: groundModel,
  platform,                                 // 内部作为 DesktopTool 面
});
```

- tick = `capture → model.ground(obs, goal) → performPlatformAction → 直到 done`。
- 绕过 SoM，不经过 `@vrover/tools` 的 mark 派发。

### 2.4 三种 Agent 的 tick 形状

| Agent 类型 | 工厂 | tick 做什么 | 工具面 |
|---|---|---|---|
| `LeaderAgent` | `createLeaderAgent` | `complete()` + `deliver_task`/`finish` | 无 observe/act |
| `GUIAgent`（SoM） | `createGUIAgent` | observe(SoM) → complete(mark) → act | 经 desktop 工具面 |
| `GUIAgent`（grounding） | `createGroundingAgent` | capture → ground(pixel) → act | Platform 封装在工具内部 |

---

## 3. 资源与工具

### 3.1 Resource 与 ResourceManager

```ts
interface Resource {
  id: string;               // 唯一标识
  capability: string;       // 人/模型可读：做什么
  exclusive: boolean;       // 独占？visual_scout = true
  kind: ResourceKind;       // 'service' | 'desktop' | 'image' | 'mcp' | 'skill' | ...
}

interface ResourceManager {
  register(resource: Resource): void;                       // 运行时加资源
  acquire(resId: string, holder: string): Lease | null;      // 独占获取（被占返 null）
  release(lease: Lease): void;
  holder(resId: string): string | undefined;                 // 当前持有者 agentId
  select(need: string): Resource[];                          // 能力查询（目前子串匹配）
}
```

### 3.2 桌面工具（visual_scout ≡ exclusive desktop）

```ts
import { createDesktopTool } from '@vrover/agent';

const desktopTool = createDesktopTool('visual_scout', realPlatform, 'exclusive desktop session');
// desktopTool extends Platform  — agent 把它当一个 Platform 调用
// desktopTool.resource — Resource{ id:'visual_scout', exclusive:true, kind:'desktop' }
```

**Platform 是工具的内部资源**：agent 注入 `DesktopTool`（它 IS a `Platform`），永远不直接持有原始 `Platform`——这与 web-search 工具内部持有 HTTP client 同构。

### 3.3 运行时注册 / MCP / skills（预留）

```ts
// 运行时加一个 MCP 服务
team.resources.register({
  id: 'my-mcp', capability: '自定义 MCP server', exclusive: false, kind: 'mcp',
});
// 加一个 skill
team.resources.register({
  id: 'code-review', capability: '代码 review skill', exclusive: false, kind: 'skill',
});
```

> 目前的 `kind` 标签已预留 `'mcp'` / `'skill'` 判别位；工具内部的具体连接/调用逻辑由对应 `kind` 的注册器实现（尚未做）。`select()` 是 naive 子串匹配——相关性强依赖 `capability` 的描述质量。

---

## 4. Task 生命周期

### 4.1 创建 → 运行 → 结果

```ts
// 1) 创建根 Task（owner 默认 = leader）
const task = team.createTask('登录网站 X 并导出账单');
// 2) 注册监听（在 run 之前）
task.on((event) => {
  // event.type: 'step' | 'capture' | 'log' | 'done' | 'error' | 'paused'
  if (event.type === 'step') console.log('step', event.step);
  if (event.type === 'capture') showImage(event.capture!.dataUrl);
  if (event.type === 'done')   console.log('result', event.result);
});
// 3) 在 team loop 下跑（自动调度：leader 委派 → worker 执行 → 结果回填 → leader 继续）
const result = await team.run(task, { maxRounds: 200 });
// result.status: 'success' | 'max_steps' | 'error'
// result.summary, result.steps
```

### 4.2 Task 可读属性

| 属性 | 类型 | 说明 |
|---|---|---|
| `id` | `string` | UUID |
| `goal` | `string` | 任务目标文本 |
| `status` | `AgentStatus` | `'idle'` → `'running'` → `'suspended'`/`'done'`/`'error'` |
| `ownerId` | `string` | 当前持有写锁的 agent |
| `history` | `readonly Message[]` | 全量会话（含截图 buffer） |
| `steps` | `readonly AgentStep[]` | 已完成的步骤（GUI worker 有 elements 计数；leader 无） |
| `result` | `TaskResult?` | 终态时设置 |
| `suspendedOn` | `readonly TaskSuspendState[]` | 挂起中的委派（fan-out 可有多个） |
| `runnable` | `boolean` | 派生：status ∈ {idle,running} && suspendedOn 为空 |

### 4.3 流式事件（`task.on(listener)`）

```ts
type TaskEvent =
  | { type: 'step';    step: AgentStep }          // 一个 step 完成
  | { type: 'capture'; capture: TaskCapture }      // 截图（SoM 标注后的 dataUrl）
  | { type: 'log';     text: string }              // 调试/进度日志
  | { type: 'done';    result: TaskResult }        // 成功
  | { type: 'error';   result: TaskResult }        // 出错
  | { type: 'paused';  result: TaskResult };       // 暂停
```

### 4.4 手动单步

```ts
await team.loop.round();  // 推进一个调度 round（leader 优先 → workers 并发各一格）
```

> 也可用 `team.loop.stop()` 在下次 round 前协作停止。

---

## 5. 创建流程（zero → running）

完整 wiring 顺序：

```
1. 模型
   chatModel = createChatModel({ id, complete, contextWindow, modalities })
   groundModel = createGroundingModel({ id, contextWindow, modalities, ground })

2. Agent 档案
   leaderProfile  = { id:'ld', role:'leader', specialties:['planning'],
                      bio:'…', models:[chatModel] }
   guiProfile     = { id:'w1', role:'worker', specialties:['gui-operate'],
                      bio:'…', requires:['visual_scout'] }

3. Agent
   leader = createLeaderAgent({ profile:leaderProfile, complete:chatModel.complete,
                                roster:{ workers:[guiProfile] } })
   gui    = createGUIAgent({ profile:guiProfile, core: createAgent({ platform, complete, ... }) })

4. 资源
   desktop = createDesktopTool('visual_scout', realPlatform)
   webSearch = { id:'web-search', capability:'搜索网页', exclusive:false, kind:'service' }

5. Team
   team = createAgentTeam({ leader, workers:[gui], resources:[desktop, webSearch] })

6. Task
   root = team.createTask('用户输入的目标')
   root.on(uiListener)

7. Run
   const result = await team.run(root)   // 阻塞到终态，或 loop 自动调度
```

---

## 6. 当前缺口（UI 需要、尚未提供）

以下项是从 UI 视角倒推、目前 `@vrover/agent` 未落地或未完整暴露的。标 🟡 为「设计已指向、尚未做」；🟠 为「设计本身尚未定」。

| 缺口 | 状态 | 说明 |
|---|---|---|
| **运行时增/删 Agent** | 🟠 | Team 创建后 `agents` 只读。`createAgentTeam` 拍平了 worker 列表——缺少 `team.addAgent(agent)` / `team.removeAgent(id)`。UI 需动态增减 worker。 |
| **资源增/删的 UI 友好 API** | 🟡 | `register` 已有，缺 `unregister`；MCP / skills 的注册器只预留了 `kind` 标签，内部连接逻辑未实现。 |
| **Team 状态快照 / 序列化** | 🟠 | 当前无 `team.snapshot()` 或持久化整队状态的路径。UI 刷新/恢复时需要。 |
| **leader 切换** | 🟠 | 设计说「一个 Team 恰好一个 leader」。如需换（如领域切换），需新的 `setLeader()` 或创建新 Team。 |
| **token / 模型延迟指标** | 🟡 | `TaskEvent` 无 metrics。UI 需展示 token 消耗、各 model 调用延迟。 |
| **步骤内截图序列** | 🟡 | `capture` 事件已发 `dataUrl`，但无结构化的「Task 完整捕获画廊」API——UI 需自己缓存 event。 |
| **Task 列表过滤/查询** | 🟡 | `team.tasks` 是 Raw Map——无 `status` 过滤、无排序。UI tab（进行中/已完成）需在应用层实现。 |
| **进度 / 预估** | 🟠 | `suspendedOn.length` 可知道「等几个子任务」，但无整体进度百分比。 |

---

## 7. 设计约束（备忘）

- **Platform 不外露**：agent 永远通过 `DesktopTool`（它 IS 一个 Platform）调用桌面。原始 `Platform` 对象封装在工具内部——与 web-search 工具内部持有 HTTP client 同构。
- **leader 不截图**：`Observes` 只归 GUIAgent。leader 的视觉来自 worker `deliver_task` 结果回传（设计决策见 [`execution-model.md` §9](execution-model.md#9-未决微问题)）。
- **独占经 `requires` + lease**：UI 为 GUIAgent 的 profile 声明 `requires:['visual_scout']`，team loop 自动做 `acquire`/`release` gate。UI 无需手动管理租赁。
- **子任务透明**：`team.createTask(goal)` 创建的是**根 Task**。子任务由 leader 的 `deliver_task` 经 team loop 自动创建——UI 可在 `team.tasks` 中看到它们，但不直接创建它们。
