# Agent 模块

> `@vrover/agent` — VRover 的大脑。本文是模块总览与概念地图；目标模型的完整论述与场景验证见 [design.md](./design.md)，**执行层（team loop / 写锁 / DeliverTask / 资源租赁）的类型草案与运行机制见 [execution-model.md](./execution-model.md)**（✅ 已落地：`src/team.ts` + `src/resources.ts`）。
> 项目级：已落地架构见 [`../../docs/architecture.md`](../../docs/architecture.md)（代码为准），长期构想见 [`../../docs/design.md`](../../docs/design.md)，待定决策见 [`../../docs/decisions.md`](../../docs/decisions.md)。

## 一句话

`AgentTeam` 是一个**多 Agent 团队 + 多类型资源池**：编排一组**角色专业化**的 Agent（各自绑定最擅长的模型），并发执行多个 Task。资源池里有多 Model、多 Tool（web-search / local-rag / bash / skills / visual_scout …）、单例全局记忆库；其中 `visual_scout` 接管一个桌面，是**独占资源**——一时刻仅一个 agent 持有。SoM 循环只是众多执行策略之一。

## 为什么走向多脑（驱动力）

1. **角色专业化 / 模型能力路由**：不同模型擅长不同事——画图、编码、多模态、本地低延迟。子任务路由到最合适的模型/Agent。（也可纯在角色层分工：同模型、不同提示 / 行为。）
2. **并发，而非串行等待**：今天的 agent 大量时间花在等单一模型推理。多 Agent 各跑各的 Task，无需相互等待。
3. **GUI agent 的双 VLM 典范**：规划 VLM（大）+ 操作 VLM（小、可本地）= `LeaderAgent` + `GUIAgent`。

> 两个压测场景见 [design.md §3](./design.md)：DeepSeek 写文章（无 GUI、同模型两角色）/ GLM 规划 + 本地 GUI-TARS 操作（异模型、像素 grounding 绕过 SoM、desktop 经 lease 独占）。

## 核心洞察

把行为拆成**可组合的纯动作接口**，**Agent 的差异 = 组合了哪些接口**；worker 按能力命名、用**子接口继承** Agent：

```
Observes   看一眼世界 → Observation（GUI=截图(+可选SoM)；纯文本=空）
Plans      规划 → Plan（Leader 的核心行为）
Acts       在平台落一个动作（永远吃像素级 PlatformAction；需一个 Platform）
Grounds    截图+提示 → 像素动作（GUI-TARS 类模型）
Completes  messages(+tools) → 文本/tool_calls（DeepSeek / GLM 类模型）

LeaderAgent extends Agent, Plans            GUIAgent extends Agent, Observes, Acts
PaintAgent extends Agent (图像模型)         …按专长扩展
```

- **Model = 连接 + 它提供的能力接口**（并非都是 `Completes`：DeepSeek/GLM 走 `Completes`，GUI-TARS 走 `Grounds`）。Team 持有、Agent 引用——可共享也可各异。
- **Task 持有会话状态 + 生命周期；一次 `exec` 做什么，委托给所绑 Agent 的组合行为**——`observe→complete→act` 不再写死，降级为某类 GUIAgent 的行为。
- **独占租赁**把「多 agent 抢同一桌面」从并发难题变成**资源语义**：`visual_scout` 一时刻仅一个 agent 持有。

## 概念地图

```
外界（app / CLI / web）  ──goal──▶  AgentTeam
┌───────────────────────────────────────────────────────────────┐
│ AgentTeam = 多 Agent 团队 + 多类型资源池 + 调度器（组合根 / Task 工厂）│
│   资源池:  Agents · Models · Tools(web-search/RAG/bash/skills)  │
│            · MemoryStore(单例·全局记忆库) · Tasks               │
│            · desktop: visual_scout (exclusive, 本地/远程)       │
│   ResourceManager: select(能力查询) / acquire·release(独占租赁) │
└────────────────┬──────────────────────────────────────────────┘
                 │ Plan → 物化成 worker 子任务，按「专长 + 资源可用性」调度（可并发 / 可接力）
   ┌─────────────┴────────────┬─────────────────┐
   ▼                          ▼                 ▼
 LeaderAgent              GUIAgent           PaintAgent
 extends Plans            extends Observes,Acts extends Agent (图像模型)
 (组合 Completes)         acquire('visual_scout')→Platform
   │  每个 Agent = profile(数据) + PromptBuilder(per-Agent) + Engine(共享协作者)
   ▼
  Task ── ContextManager  (per-Task：history + 按模型调窗口)
          PromptBuilder   (per-Agent：按专长建提示 + 从资源池注入相关 Tools)
          MemoryManager   (per-Task 快照)  ◀── MemoryStore (Team 级全局)
```

**所有权**：Team 拥有资源池 + 单例 MemoryStore；Task 拥有 ContextManager(1:1)；Agent 拥有 PromptBuilder(1:1)；独占资源 `visual_scout` 一时刻租赁给 ≤1 Agent。

- **同一时刻**：一个 Agent 只跑一个 Task（角色层不并发）。
- **Task 生命周期内**：可在 Agent 间接力（规划 → 操作 → 遇阻回规划）。

## 与代码现状的映射

| 概念 | 现状 | 性质 |
|---|---|---|
| **AgentTeam**（资源池 + 调度 + Task 工厂） | 无 | 🆕 新顶层（吸收现有 `Agent` 的工厂职责） |
| **Resources / ResourceManager**（共享/独占租赁） | 无 | 🆕 资源池 + lease |
| **动作接口** `Observes/Plans/Acts/Grounds/Completes` | `observe()`/`act()`（`step.ts`）已拆；`Plans/Grounds` 无 | 🔼 抽成可组合接口 |
| **Agent 子接口** `Leader/GUI/Paint… extends Agent` | `Agent`（`core.ts:120`，单脑） | 🔼 重塑为角色接口 + 子接口继承 |
| **Model**（`Completes`/`Grounds`/`Generates`） | `CompleteFn`（`@vrover/llm`） | 🔼 提升为对象；能力分叉 |
| **PromptBuilder**（per-Agent·Team-aware·注入 Tools） | `PromptRegistry`（`prompts/`） | 🔼 扩展为 per-Agent + 资源池查询 |
| **ContextManager**（per-Task·按模型调窗） | `pruneForModel`（`context.ts:29`，纯函数） | 🔼 提升为有状态对象 |
| **MemoryStore**（Team 级单例·全局） | `FileMemoryManager` 部分 | 🆕 全局层 |
| **MemoryManager**（per-Task 快照） | `MemoryManager` / `FileMemoryManager` | ✅ 吻合 |
| **Task**（委托 step 形状给 Agent） | `Task`（`core.ts:151`，写死 SoM 循环） | 🔼 解耦 step 策略 |
| **visual_scout 独占资源** | `RemotePlatform`（`remote.ts`，每连接一个） | 🔼 收进池 + lease 语义 |

完整论述、TS 接口草案、演化路径与未决问题见 [design.md](./design.md)。

## 设计原则（延续项目既有 + 本包风格）

- **面向数据的作用域与生命周期**（本包 `CLAUDE.md`）：核心状态 = 纯数据接口；行为 = 组合纯动作接口；子类型用 interface extends；实现藏在 `createX(deps)` 后、`destroyX(ins)` 收尾。无 IOC 容器即达 SOLID。
- **组合优于继承（类层面）**：Agent / Team 都是组装协作者与动作接口；接口继承只声明能力组合。
- **纯核脏壳**：构造无副作用；密钥 / 连接在 `Model`，文件在 `MemoryStore`，OS / 桌面在 desktop 资源的 Platform；核心可注入假模型 / 假资源零成本单测。
- **演化，非重写**：多脑是单脑的**泛化**——单 Agent、无资源池的 Team 即今天的单脑路径（向后兼容）。逐步提升现有最小缝，每步保持测试全绿。

## 与 graph walker 的关系

**正交，各自推进。** 多 Agent 自洽于本模块；graph walker（按 node 动态注入高层操作）继续在 `@vrover/scout` 按项目 M1 推进。未来 walker 的「当前 node + 高层操作」会作为**数据源**注入 `PromptBuilder`，或由一个 GUIAgent 持有——但那是复合，不是依赖。
