# VRover 架构构想（Design Vision）

> **状态：构想阶段，方向性设计。** 已落地的代码架构见 [architecture.md](./architecture.md)；待定的设计决策见 [decisions.md](./decisions.md)。

## 一句话

把 Visual Scout 做成一个**独立运行的本地 server**：它接入某个图形窗口 / 远程桌面 / 手机，把应用建模成一张 **UI 图**（每屏一个 node，视图切换控件是 edge），以 **graph walker** 的形式为 AI 提供从底层（click / type）到高层（`go_back`、`open_article(id)`）的操作；用纯文本 **DSL** 持久化整张图，AI 能基于 DSL 为新应用生成 / 扩展图并反哺工具；同时对外提供**人类 JS 脚本 API**。

## 核心构想

1. **胖工具 / 本地 server**：Visual Scout 独立进程运行，负责真实接入图形目标。语言无关、可远程、可独立重启、可同时服务多个客户端。
2. **UI 图 + graph walker**：`node` = 一个界面 + 其可交互控件；`edge` = 由「视图切换控件」维护的界面跳转。walker 在图上导航、执行操作。
3. **分层操作 + 动态注入**：底层 `click / type / scroll / keypress` 之上，提供 `go_back`、`open_article(article_id)` 等高层操作；这些操作**按「当前 node」动态注入** AI 当轮上下文。走高层操作时，AI 甚至不必关心具体 UI。
4. **DSL 持久化 + AI 反哺**：纯文本 DSL 存整张图；AI 面对新应用时，可基于 DSL 语法**生成 UI 图**，反哺工具，形成闭环（探索 → 建图 → 持久化 → 复用）。DSL 既是 SoM 方案之一，也持久化高频 APP 的 UI 结构、省重复计算。
5. **人类脚本 API**：同一套图 + 操作对外暴露 JS 接口（今天的 `@vrover/scout-client`），程序员可编写自动化脚本；脚本能沉淀 / 修正图，供 AI 复用。
6. **宏录制**（构想）：由人类操作示范，AI 分析行为，生成脚本，最终持久化为连续自动化脚本，处理高度重复化的内容。

## 组件分解（三大件）

整个系统由**两大件 + 一个暂缓依赖**构成（详见 [decisions.md](./decisions.md) D10）：

| 组件 | 职责 | 现状 |
|---|---|---|
| **VRover** · GUI agent（大脑） | agent loop；任务理解 / 规划；调用**视觉模型服务**（多模态大模型）思考；经 walker 操作 UI，走不通回退 SoM | `@vrover/agent` + `@vrover/llm` 已落地 |
| **Visual Scout** · 胖工具 | ① UI 交互（截图 + 键鼠）② **graph map**（共享 · 持久的应用图）③ **walker**（每连接一个，会话状态）④ grounding：capture → elements，靠**内部传统 CV + OCR**（廉价 · 实时） | ①④ 有雏形（`@vrover/scout` + `@vrover/platform` + `@vrover/som`）；②③ + CV/OCR 未实现 |
| **视觉模型服务**（多模态大模型） | VRover 大脑调用的**多模态大模型**（第三方厂商提供，如 Claude）；能看图 / 推理 / 决策 | **暂用第三方**，后期再议自训练 |

> ⚠️ **关键澄清**：「视觉模型服务」= VRover 的**大脑大模型**（多模态），**不是** grounding 服务。Visual Scout **不调**多模态大模型——它的 ④ grounding 全靠**内部传统 CV + OCR**（廉价、实时）。因此识别流水线全程廉价，**唯一昂贵资源是多模态大模型**；图优先导航的意义就是省它的调用。

关键区分：**graph map 是知识**（持久、跨连接共享、DSL 存盘，一个应用一份）；**walker 是会话状态**（per-connection，记当前 node + 遍历栈 + 当前界面状态，一个连接一份）。多个 VRover 连接共享一份 graph map、各持一个 walker——这正是 Visual Scout 多会话 server 的形态（已落地：每条 TCP 连接一个 session + walker 占位）。

## 与现有代码的关系（演化，非重写）

> 组件分解见上方与 [decisions.md](./decisions.md) D10。当前代码已拆成 pnpm workspace（见 [architecture.md](./architecture.md) 包结构），演化路径清晰：

- `@vrover/platform`（capture / click / type…）→ Visual Scout ① 的**最底层驱动**，walker 用它实际操作应用。
- `@vrover/som` SoM 标注 → Visual Scout ④ grounding 的**感知引擎**：建图时识别控件、走图失败时重新定位；承载「无障碍树/DOM + 传统 CV/OCR」（见 [som.md](./som.md)）。
- `@vrover/agent` 的「可注入 `complete`」已就位；「按 node 动态注入工具」**尚未**在 loop 契约里（见 [architecture.md](./architecture.md)「还没有 walker 的位置」与 [decisions.md](./decisions.md) D8）。
- Visual Scout 已 server 化：`@vrover/scout`（server）+ `@vrover/scout-client`（第三方 SDK）+ `@vrover/scout-protocol`（线协议）；`RemotePlatform`（大脑侧 client）在 `@vrover/agent`。
- `done` / `TaskResult` 等契约不变。

## 执行模型：图优先，视觉兜底

- **已知屏走图**：确定性、快、不必每轮看屏。每步先尝试把当前界面匹配成 graph map 中的已知 node（L0 nativeId / L1 图像层 / L2 结构签名，详见 [decisions.md](./decisions.md) D1）；命中即复用库存元素与已知边，连 grounding 都可能省掉。`go_back` 几乎白捡——walker 维护遍历栈，`go_back` = pop。
- **未知 / 漂移回退 SoM**：图里没有当前 node，或记录的控件已消失时，回退 Scout 内部 CV/OCR 感知（L3 全量识别）重新定位 / 建边。
- 这把「纯图」与「纯视觉」的两难解掉：图负责高频已知路径，视觉（CV/OCR）负责探索与自愈；多模态大模型只在 VRover 真要思考时才调。

## 关键概念

| 概念 | 说明 |
|---|---|
| **node** | 一个界面 + 控件集 + 一个稳定签名（用于判同） |
| **edge** | 从 node A 经某控件跳到 node B（记录触发控件 + 效果） |
| **高层操作** | 按 node 暴露的语义动作：结构相对型（「点列表第 1 项」）或语义标签型（`open_article(id)`） |
| **遍历栈** | 支撑 `go_back` / 历史回溯 |

## 增量路线（最小验证先行）

| 里程碑 | 内容 | 验证什么 |
|---|---|---|
| **M1（建议先做）** | UI 图数据模型 + DSL 草案 + **in-process** graph walker；把现有 Mock 扩成多屏小应用（登录→主页→文章列表→文章） | 建图、`go_back` 沿边回退、按 node 动态注入高层工具。不接真平台，全复用现有 Platform / loop |
| **M2** | 节点身份签名 +「图优先 / 视觉兜底」降级路径 | 走图失败时自动回退 SoM 重新定位 |
| **M3（部分已落地）** | server 化（定义协议，独立进程 / 可远程），brain 经 client 接它 | 工具与大脑解耦、可远程驱动。✅ TCP server + 客户端 SDK + 会话已落地；剩 walker 接入 |
| **M4** | AI 闭环（基于 DSL 生成 / 扩展图，校验合并）+ 人类 JS 脚本 API | 新应用自动建模、脚本沉淀图。✅ 脚本 API（`@vrover/scout-client`）已落地 |
| **M5** | 真平台接入（桌面 / 手机 / 浏览器） | 落地真实自动化 |

> 节点身份、DSL 语法、高层操作语义起点等 M1 的前置硬骨头与待定决策，全部在 [decisions.md](./decisions.md) 逐个拍板。

## 参考脉络（便于对照研究）

> 这些都是把「应用建模成可导航图」思路的不同侧面，研究时可对照其节点身份、建图策略、操作抽象。

- **Model-based GUI 测试**：把应用建模成 FSM / 状态图，按模型生成测试路径。代表：**Stoat**、**GraphWalker**、Appium + 模型。
- **AutoDroid**：LLM + app「常识知识图」+ function tree。
- **AppAgent**：探索阶段为每个元素建「操作文档」知识库，先学后用。
- **App Crawler / Android Monkey**：自动化遍历建图（无语义，覆盖导向）。
- **Mobile-Env / AndroidWorld**：交互环境 + 任务评测。
- 高层操作 ≈ 强化学习里的 **options / skill library**；DSL ≈ 面向自动化的程序合成。

VRover 的差异化：**感知（CV/OCR）+ 图（UI graph）+ 分层操作（按 node 动态注入）+ DSL（AI 反哺闭环）+ 人类脚本** 一体化。
