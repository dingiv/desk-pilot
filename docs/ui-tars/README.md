# UI-TARS / UI-TARS-desktop 借鉴待办（study → TODO）

> 本目录是把 **UI-TARS-desktop**（TS/Electron 多模态 agent 全家桶）与 **UI-TARS**（ByteDance 的 GUI agent 模型仓 + Python 推理/动作解析）里**值得 VRover 借鉴的架构与设计**，整理成带**精确出处**的待办清单。
>
> 目的：日后做某条待办时，能直接跳回原实现去看「人家到底怎么写的」。
>
> 与本仓其它文档的关系：`docs/architecture.md`（as-built）、`docs/design.md`（远期 UI 图构想）、`docs/decisions.md`（D1–D11）。本目录是**外部参考**，不改变 VRover 自身决策，只提供可借鉴的实现范本。

## 源码定位

研究对象的两个仓（都在 `/workspaces/gui_agent/` 下）：

| 仓 | 路径 | 性质 |
|---|---|---|
| **UI-TARS-desktop** | `UI-TARS-desktop/` | TS pnpm monorepo。三大子栈：`apps/ui-tars`（Electron 桌面应用）、`packages/ui-tars`（GUI agent SDK + operators）、`multimodal/`（统一新栈：`gui-agent` / `tarko` / `agent-tars`） |
| **UI-TARS** | `UI-TARS/` | Python。`codes/ui_tars/`（动作解析 + 提示词，**权威参考**）+ 论文 PDF |

每条待办的「参考出处」写成 `仓/相对路径#函数名或类名`（带行号的尽量保留）。下文出现的 `gui-agent`、`tarko`、`agent-tars` 均指 `UI-TARS-desktop/multimodal/` 下的子工作区。

## 目录组织（按 VRover 子系统分文件）

| 文件 | 主题 | 映射到的 VRover |
|---|---|---|
| [01-platform-operator.md](./01-platform-operator.md) | Platform / Operator 多后端抽象、坐标、能力矩阵 | `@vrover/platform` + `crates/drivers` |
| [02-agent-loop-exec-goto-pause.md](./02-agent-loop-exec-goto-pause.md) | Agent 循环、事件流、回合制、中断、流式双模式 | `@vrover/agent` `runAgent` + 在做的 `Agent{exec,goto,pause}` |
| [03-action-vocab-thought-prompt.md](./03-action-vocab-thought-prompt.md) | 动作词表、推理链、smart_resize、提示词 | `@vrover/tools` + `prompts` + 截图管线 |
| [04-llm-provider-streaming-mcp.md](./04-llm-provider-streaming-mcp.md) | Provider 抽象、流式签名、ToolCallEngine、MCP 工具协议 | `@vrover/llm`（`CompleteFn`）+ `@vrover/tools` |
| [05-server-snapshot-context.md](./05-server-snapshot-context.md) | server 形态、会话存储/恢复、快照录制回放、上下文工程 | `@vrover/scout`（server）+ `Agent.goto` + `pruneForModel` |
| [06-web-ui-interrupt-viz.md](./06-web-ui-interrupt-viz.md) | SSE 流式、统一 abort、会话状态、可视化、replay | `apps/visual_rover_web` + scout devtools + `Agent.pause` |

## 优先级图例

- **P0** — 直接服务于当前在做的 `Agent` 接口（exec / goto / pause / 流式），建议优先。
- **P1** — 高价值架构改进（平台抽象、循环健壮性、provider 流式）。
- **P2** — 锦上添花 / 远期（MCP、replay 分享、多存储后端等）。

## 三个跨切面结论（动手前必读）

1. **VRover 的 SoM + tool_calls 已经绕开了 UI-TARS 的两大复杂源**：自由文本动作解析（parser chain）、归一化坐标↔像素映射。**不要盲目移植** `action-parser` 的解析器链或坐标系——那是 UI-TARS 走「自由文本 + 裸坐标」范式的代价。要借鉴的是**动作词表设计、推理链、完成/中断信号语义、截图 smart_resize**（见 [03](./03-action-vocab-thought-prompt.md)）。

2. **tarko 没有 `goto` / 历史分支能力**（Lane 5 已穷举确认）。`agent-snapshot` 是**为测试服务的录制/回放**（每个快照 = 一个独立测试用例，不能从某历史点 fork 新对话）。所以 VRover 在做的 `Agent.goto`（回到历史 chat 点分支重跑）是**净新设计**——tarko 只提供「录制 / 恢复 / 校验」的**底座**可借鉴，分支逻辑得自研。详见 [05](./05-server-snapshot-context.md)。

3. **VRover 已有、无需重复的**（避免无谓待办）：
   - observe→think→act 循环（`packages/agent/src/core.ts#runAgent`）
   - Set-of-Mark 标注（`@vrover/som`）
   - function-calling / tool_use 块（`@vrover/llm` + `@vrover/tools`）
   - Platform 接口 + Mock/Desktop/Remote 三实现（`@vrover/platform`）
   - 独立 TCP server + per-connection session（`@vrover/scout`，二进制协议、PNG 直传无 base64，**比 tarko 的 HTTP/JSON 更省**）
   - 滑动窗口上下文压缩 + 截图封顶（`packages/agent/src/context.ts#pruneForModel`）
   - devtools HTTP+SSE 调试面（`@vrover/scout` devtools）

> 凡是上面已列的，下文待办里不再单列「从零做」，只列**增强/补缺**。
