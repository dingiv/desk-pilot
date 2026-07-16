# 设计决策记录（decisions.md）

> 架构决策日志。每条决策写 **现状 · 选项 · 倾向**；定稿的标 ✅，待定的标 🟡。
> 现状架构见 [architecture.md](./architecture.md)，长期构想见 [design.md](./design.md)。

---

## 🟡 D1 — 节点识别方案（建图地基）

**问题分层**：每一步都捕获一张截图，必须回答「这是图里某个已知 node 吗」。这不是一次性建图，而是**每步都跑的识别**——所以必须**先便宜地匹配已知，匹配不上才上昂贵的全量视觉识别**（即 design.md「图优先 / 视觉兜底」在识别子问题上的落地）。难点：滚动 / 动态内容 / 加载态让"同一屏"长得不一样，不同屏又可能结构相似。

### 基石：node = 它的「UI 描述」（NodeProfile），描述是通用货币
- 一个 node **就是**它的 UI 描述；**Visual Scout 负责定义和产出描述**；识别器比较描述；图存描述。三者必须用同一种描述形态，否则没法比。
- 因此 **D1（怎么匹配）、D9（描述从哪来 / grounding）、[som.md](./som.md)（描述长什么样）是一个耦合簇，不能单独定 D1。**
- 倾向的描述形态 `NodeProfile = { nativeId?, imageHash, structuralSig, elements }`——native id（有则带）、图像层指纹、结构签名、元素表（供动作解析）。建新 node 时 Visual Scout 全量识别产出它；后续每步拿它去比。

### 识别流水线（最便宜优先，逐层短路）
```
每步截图
  ├─ L0  nativeId（免费元数据，平台给就有）          命中 ⇒ 已知 node
  ├─ L1  图像层快匹配（裸截图 pHash/embedding vs 库）  命中 ⇒ 已知 node   ← 纯图像识别，不感知
  ├─ L2  结构匹配（取元素树 → 结构签名 vs 库）         命中 ⇒ 已知 node   ← 要 grounding，但便宜档（AT-SPI/DOM）
  └─ L3  全量识别（Scout 内部 CV/OCR，廉价·实时）       ⇒ 新 node，产出 NodeProfile 入库
```
- **L1 是上一版 D1 漏掉的一层**：识别不应假设"已经拿到元素树"。先拿**裸截图**做图像层匹配（即"图像识别与匹配技术"），命中就能跳过感知。
- **L3 才是全量识别**：L0–L2 都不命中时，跑 Scout 内部 **CV/OCR** 把截图完全感知出来，建新 node（廉价、实时，**不调多模态大模型**）。整个 L0–L3 全程廉价，唯一昂贵资源是 VRover 的多模态大模型——图优先导航省的就是它。
- 这套流水线让「图优先不必每轮看屏」成立：已知屏走 L0/L1 命中，连 L2 grounding 都可能省掉。

### L2 的结构签名怎么算（保留归一化规则）
核心思想：**用相对布局 + 角色多重性，丢掉绝对坐标和易变内容文本**。
1. **按阅读序排序**：`(rowBand, colBand)`，`rowBand=floor(y/bandH)`，`bandH≈屏高/20`（同一视觉行落同一 band，容忍 y 抖动）。
2. **降维每个元素**为 `(role, labelClass)`：`role`=无障碍角色小写；`labelClass` **不用原文**，按类型归类 `empty|icon|short-text|long-text|numeric|datetime`（stable 角色可留归一化原文做弱辅助）。
3. **保留多重性**（3 button ≠ 5 button）、**丢绝对坐标**（只留 band 内相对序）。
4. 指纹 = 排序后 `(role,labelClass)` 序列拼接 / hash，如 `[input:short, input:short, button:short]`。

### 相似度判定（L2 判同）
- **精确**（结构签名相等）⇒ 同 node。
- **近似**（Jaccard / 编辑距离 ≥ τ≈0.85）⇒ *候选*同 node；需 nativeId 也一致才合并，否则记"漂移边"。
- **低于 τ** ⇒ 不同 node（→ 进 L3）。

### 滚动 / 动态内容怎么不误判
- 描述只算 **稳定 chrome（顶栏 / 导航 / 底栏）+ 内容区形状（元素数 + 角色多重性）**，不算内容文本。同 chrome + 同形状、列表项不同 ⇒ 同一个（参数化）node。
- 滚动位置作 node **子状态**或自环边（scroll edge），不单独建 node。

### 示例（对照 Mock 登录界面）
三元素 `[input]Username [input]Password [button]Login`，无 nativeId：
- L0 跳过；L1 裸截图 pHash 命中库里 login 截图 ⇒ 已知 login node（连元素都不必重取）。
- 首次见 login 时走 L3 建库：L2 签名 `[(input,short),(input,short),(button,short)]` → `S_login`。
- 登录后主页 → `S_home≠S_login`，L0–L2 不命中 → L3 建 home node，记 login→home edge（触发 = Login button）。
- `go_back` → pop 栈 → login。

### 待定的子问题（🟡）
- **L1 图像匹配用什么**：(a) 感知哈希 pHash/dHash（确定、几乎免费、但对布局/滚动脆弱）；(b) 学习型 embedding（要模型、稳）；(c) 跳过 L1、结构优先。倾向先 (a) 上 mock，稳不住再 (b)。
- **命中已知 node 后元素从哪来**：复用库存 elements（省 grounding，漂移风险）还是每步重取（稳但费）？倾向：L1/L2 高置信命中 ⇒ 复用库存；低置信或动作前 ⇒ 重取。依赖 D9。
- **描述模型归谁定义**：倾向 Visual Scout 拥有 `NodeProfile` 定义，D9 定 grounding 如何填充它。
- 加载态 / 动画中途抓拍：连抓两帧 diff ≤ ε 才采（属 L3 实现细节）。

**状态**：🟡 草案 v2（加入 L1 图像层 + NodeProfile 货币概念）。L1 选型 + 元素复用策略待定，需与 D9 一起拍。

---

## 🟡 D2 — DSL 语法（序列化 + AI 反哺的载体）
**现状**：整个反哺闭环 + 人类脚本都依赖它，但至今**一行 DSL 都没写**。
**选项**：(a) 缩进式（YAML 风格，人友好）；(b) S-expr / 类 DSL（结构紧凑、好解析）；(c) 直接 TypeScript / JSON 子集。
**倾向**：待定。先在 M1 定一个最小能表达 node + edge + 参数化的草案。
**依赖**：D1（签名怎么进 DSL）、D3、D5。

## 🟡 D3 — 高层操作语义起点
**现状**：纯结构相对（"点列表第 1 项"、"返回上一屏"）够不够起步？还是一开始就要 AI 给控件打语义标签（article / submit）支撑 `open_article(id)`？
**选项**：(a) 先纯结构相对，语义标签作 DSL 里可选层后加；(b) 一步到位语义标签。
**倾向**：(a)——先结构相对跑通 walker，语义标签后置。

## ✅ D4 — Visual Scout 进程位置（已定：独立 TCP server + 会话）
**结论**：Visual Scout 是**独立 TCP server**，说一种**自定义二进制协议**（12 字节帧头 `[magic][ver][type][id u32][len u32]` + JSON/binary payload）。客户端先发 `HAND_SHAKE` 握手，server 据此为每条连接铸造一个 **session**（含独立 `Platform` 操作终端＝截屏器＋键鼠 + walker 占位）；server 级持有共享 `GraphMap` 占位。对应 D10 的「N 连接 ⇒ N session ⇒ 共享 1 份 graph map」。walker 仍 per-session（在 Scout 内，非大脑侧）。
**演进**：从早期「in-process 优先（M1）、server 化留到 M3」直接跨到 server 形态——多会话模型本身要求 server。`Walker` / `GraphMap` 为**空占位**，逻辑等 D1/D2 定稿。
**状态**：✅ server 形态已定（TCP + 自定义协议 + 握手建会话）；🟡 walker / graph-map 内部随 D1/D2 细化。详见 [scout-server.md](./scout-server.md)。

## 🟡 D5 — 参数化节点（`open_article(id)` 的 id→项映射）
**现状**：`open_article(id)` 意味着"文章列表"是结构固定、内容可变的节点。要把 id 映射到具体项，DSL 必须支持参数化节点，否则退到"点第 N 项"。
**倾向**：待定。依赖 D2（DSL 是否支持参数）、D3。

## 🟡 D6 — DSL 合并与自愈
**现状**：AI 反哺生成的图怎么校验、合并而不破坏已有；应用升级导致控件漂移时怎么容忍（丢了走视觉兜底重建边）。
**倾向**：待定。依赖 D1（签名决定漂移能否被识别）、D2。

## 🟡 D7 — 探索预算
**现状**："穷举常用 UI"是理想，现实是有界贪心探索 + 任务驱动增量。
**倾向**：任务驱动增量为主，有界贪心兜底。

## 🟡 D8 — loop 契约要不要加 walker / 工具注入钩子（架构）
**现状**：见 [architecture.md](./architecture.md) 的 over-claim。`tools` 现硬编码为 `TOOL_DEFS`，loop 无 node 概念。
**选项**：(a) 加 `AgentOptions.getTools?(ctx)` 钩子按 node 动态返回工具；(b) walker 直接接管 act 阶段（已知边不过 LLM）；(c) 两者都要。
**倾向**：(c)——getTools 做动态注入、walker 做已知边直行，分层。M1 先做 (a)。

## 🟡 D9 — grounding 入口收敛（`Platform.getElements` vs `GroundingSource.detect`）
**现状**：两套元素来源入口重叠，文档说 `getElements` 暂代 `GroundingSource`。
**选项**：(a) grounding 留在 Platform 里；(b) 抽成独立注入件 `GroundingSource`，Platform 只管 capture + act。
**倾向**：(b)——node 签名（D1）本质是 grounding 关切，抽出来 walker 拿签名原料更顺；且 grounding 负责填充 D1 的 `NodeProfile`（UI 描述 = 识别货币）。M1 之前最好先定。

---

## ✅ D10 — 系统组件分解（三大件边界）

**结论（方向已定）**：整个系统 = **两大件 + 一个暂缓依赖**。

| 组件 | 职责 | 现状 |
|---|---|---|
| **VRover**（GUI agent / 大脑） | agent loop；任务理解 / 规划；调用**视觉模型服务**（多模态大模型）思考；消费 walker 给的「当前 node + 高层操作」，走不通回退 SoM | `@vrover/agent` + `@vrover/llm` 已落地 |
| **Visual Scout**（胖工具） | ① UI 交互：截图捕获 + 键鼠注入（= Platform 原语）② **UI graph map**：共享 · 持久的应用图（node/edge/DSL）③ **UI walker**：每连接一个，维护当前界面状态 + 遍历栈 ④ grounding：capture → elements，靠**内部传统 CV + OCR**（廉价 · 实时）→ NodeProfile | `@vrover/scout`（+ `-protocol`/`-client`）+ `@vrover/platform` + `@vrover/som` 是 ①④ 的雏形；②③ + CV/OCR 未实现（= M1 / M3） |
| **视觉模型服务**（多模态大模型） | VRover 大脑调用的**多模态大模型**（第三方厂商提供，如 Claude）；能看图 / 推理 / 决策 | **暂用第三方**，后期再议自训练；本轮不展开 |

> ⚠️ **关键澄清**：「视觉模型服务」= VRover 的**大脑大模型**（多模态），**不是** grounding 服务。Visual Scout **不调**多模态大模型，④ grounding 全靠内部 CV/OCR。

**关键区分（graph map vs walker）**：
- **graph map 是知识**：持久、跨连接共享、DSL 存盘。一个应用一份。
- **walker 是会话状态**：per-connection，记"我现在在哪个 node、遍历栈、当前界面状态"。一个连接一份，多处导航。
- N 个 VRover 连接 ⇒ N 个 walker ⇒ 共享 1 份 graph map（Visual Scout 作为多会话 server 的形态 = M3）。

**依赖关系**：VRover 依赖**视觉模型服务**（多模态大模型 = 大脑）+ **Visual Scout**（UI 操作）。Visual Scout 与多模态大模型**无直接调用**——④ grounding 是内部 CV/OCR。唯一昂贵资源是多模态大模型；图优先导航的意义就是**省它的调用**（已知屏走 walker，可完全不调大模型）。

**对其它决策的影响**：
- **D4**（walker 位置）：walker **就在 Visual Scout 里**、per-connection。D4 剩下的只是"Visual Scout in-process 还是独立 server"。
- **D8**（loop 工具注入钩子）：VRover 经 walker 拿高层操作，walker 是动态工具注入的来源。
- **D9**（grounding）：grounding 属 Visual Scout ④，负责填 NodeProfile。
- **D1**（L3 全量识别）：L3 由 Scout 内部 **CV/OCR** 完成（廉价、实时），**不调多模态大模型**。

**M1 代码结构提示**：即使 in-process 单连接，也应把 `GraphMap`（数据 + 持久化）与 `Walker`（会话状态 + 导航）分开成两个对象——graph 是共享知识，walker 是一次性会话状态，混在一起会挡住 M3 的多会话。

**状态**：✅ 组件边界已定（三大件）；🟡 各件内部（walker / graph 的 API 形态）随 M1 细化。

---

## ✅ D11 — 感知技术分层（CV/OCR + ML 检测，均为备选）

**结论**：Visual Scout 的 ④ grounding 用**三档感知源，逐档兜底**，全部作为备选保留（不做"只选一个"的二选一）：

| 档 | 技术 | 成本 | 鲁棒性 | 时机 |
|---|---|---|---|---|
| 1 | 无障碍树 / DOM | 免费 · 精确 | 依赖平台暴露 | 默认首选 |
| 2 | 传统 CV + OCR | 廉价 · 实时 · 无权重 | 风格化图标 / 自绘控件脆 | 1 拿不到时（M1 先做）|
| 3 | ML 视觉检测（OmniParser 类，onnxruntime）| 要权重 + 算力（CPU 接近实时）| 鲁棒，判交互性 / 语义 | 2 拿不准时备选 |

- 三档**输出形态一致**（框 + 标签 → NodeProfile.elements）、**都跑在 Scout 本地**、**都不是多模态大模型**。逐档兜底 + 合并去重。
- **M1 只做 1+2**（CV/OCR 验证廉价实时档）；**3 作为备选鲁棒档**，先用现成权重，后期再议自训练。
- **ML 视觉检测 ≠ 视觉模型服务**（多模态大模型）：前者是 Scout 本地小专用模型找控件框，后者是 VRover 大脑。别混。

技术细节见 [som.md](./som.md)「感知技术分层」。

**状态**：✅ 三档全保留为备选，逐档兜底；🟡 第 3 档具体模型 / 自训练时机待 M1 后再议。
