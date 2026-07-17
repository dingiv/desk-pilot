# 现状架构（代码为准）

> 本文记录**已经落地**的架构与契约，对照代码。长期方向见 [design.md](./design.md)；待定的设计决策见 [decisions.md](./decisions.md)；SoM 子模块见 [som.md](./som.md)；Scout server 见 [scout-server.md](./scout-server.md)。
> 状态：pnpm monorepo（`packages/*`），首版里程碑（骨架 + 循环 + mock + Scout TCP server + 客户端 SDK）已落地，43 个测试全绿。

## 一句话

agent 跑一个 **observe → think → act** 循环：每步截图 + 取元素 → SoM 画编号框 → 把「标注图 + 元素表 + 工具」交给 LLM → 执行模型挑的 mark 对应的元素。两层可插拔缝：`Platform`（驱动什么目标）+ `CompleteFn`（用什么大脑）。

```
agent loop（observe → think → act）
   ├─ observe: Platform.captureScreen() + Platform.getElements() → SoM.annotate()  （带编号框的图 + 元素表）
   ├─ think:   LLM（@vrover/llm；可插拔）
   └─ act:     工具执行器：mark → 元素 → 中心坐标 → Platform 原语（click / type / scroll / keypress）
```

核心设计：**action 用 SoM「编号(mark)」引用元素，而非裸坐标**。模型只挑编号，定位精度交给真实的元素边界框；`Platform` 保持坐标导向，贴合真实鼠标键盘。

## 包结构与依赖（pnpm workspace，无环）

```
@vrover/scout-protocol  (leaf)  线协议（二进制帧 + 消息 + UiElement/Bounds）—— client 与 server 共享契约
@vrover/scout-client    → scout-protocol            第三方客户端 SDK（只依赖协议；面向第三方开发人员）
@vrover/platform        → scout-protocol            Platform 接口 + Mock/MultiScreen/Desktop
@vrover/som             → platform                  SoM 标注 + 元素表
@vrover/llm             (leaf)                      Anthropic 适配器 + loadConfig
@vrover/tools           → platform, som, llm        工具定义 + mark→坐标 执行器
@vrover/scout           → scout-protocol, platform  Visual Scout TCP server（会话化）
@vrover/agent           → platform, llm, som, tools, scout-client   runAgent 循环 + RemotePlatform
```

关键边界：

- `UiElement`/`Bounds` 定义在 **`@vrover/scout-protocol`**（让 client 只依赖协议）；`@vrover/platform` re-export，下游照旧从 platform 取。
- **`RemotePlatform`**（大脑侧 TCP client，薄包 `ScoutClient`）在 **`@vrover/agent`**——它是项目内唯一消费 `scout-client` 的地方，client 因此对第三方完全独立。
- 开发用 **source-resolving exports**（`exports → ./src/index.ts`）：`tsx`/`vitest` 直读 TS，无需 build；`pnpm build` 经 TS project references 给各包产出 `dist/`。

## 契约边界（精确版）

### `Platform`（`@vrover/platform`）— 目标抽象
最底层驱动，坐标导向（贴近真实鼠标键盘）：`captureScreen(): Promise<Screenshot>`、`getElements(): Promise<UiElement[]>`、`performClick(x,y)` / `performType(text)` / `performScroll(x,y,dir)` / `performKeypress(keys)`。

> ⚠️ **`getElements` vs `GroundingSource.detect` 重叠**：`@vrover/platform` 里还有一个**未接线**的 `GroundingSource` 缝。节点身份签名（建图地基）本质是 grounding 关切——walker 也要拿元素结构做签名。grounding 最终是留在 Platform 里、还是抽成独立注入件，是 [decisions.md](./decisions.md) 的 D9 待定项。

### `CompleteFn`（`@vrover/llm`）— 大脑出口
`(req: CompleteRequest) => Promise<LLMResponse>`，loop 拿它当依赖注入，测试用假 LLM 零成本跑。今天只有 `anthropic.ts` 实现；加 provider = 加一个同签名函数。`loadConfig`（读环境变量）也归本包（其唯一消费者 `anthropic.ts` 在此）。

### SoM（`@vrover/som`）— 感知
`annotate(screenshot, elements) → { annotated PNG, table }`。模型说「点 mark 3」，执行器把 mark → 元素 → `centerOf(bounds)` → `Platform.performClick`。详见 [som.md](./som.md)。

### Action / 工具面（`@vrover/tools`）
工具一律 **mark-only**：`click(mark)` / `type(mark,text)` / `scroll(mark,dir)` / `keypress(keys)` / `done(summary)`。schema 用 Zod 写一次 → 转 JSON Schema 给模型，单一来源不漂移。

### agent loop（`@vrover/agent`）
每步：capture → getElements → annotate → 塞进 history → `complete(...)` → 逐个 `dispatch` tool call → tool_result 塞回 history。`done` 或 maxSteps 或 LLM 报错时停。`TaskResult { status, summary, steps }`。

### Visual Scout（`@vrover/scout` + `-client` + `-protocol`）
独立 TCP server，说一种自定义二进制协议；客户端先握手建立会话，每个会话拥有独立操作终端（一个 `Platform` = 截屏器 + 键鼠）。详见 [scout-server.md](./scout-server.md)。

## 原生驱动层（Rust `crates/`）

仓库根新增了一个 **Cargo workspace**（`crates/`），与 pnpm TS monorepo 并存、互不干扰。这是填 `NativeLayer` 那块预留 Rust 缝的**真正原生层**（取代 `playground/nutjs`/`pyautogui` 的 JS/Python 平替）。详见 [`crates/README.md`](../crates/README.md)。

核心拆分（**截屏与键鼠分离**，两条独立 trait）：

只有一个 crate `vrover-drivers`：截屏/键鼠是它内部两条独立 trait，三个平台后端是 feature-gated 模块（默认关，所以无原生依赖也能全测）。pipewire/uinput/libei 原本是三个独立 crate，现已合并进来。

| 模块 (feature) | 角色 | 状态 |
|---|---|---|
| `drivers` 核心 | `CaptureSource` + `InputSink` trait、`Frame`/`Button`/`Key`/`DriverError` + 测试桩 | ✅ 全测（本容器） |
| `backends::pipewire` (`pipewire`) | `CaptureSource` via PipeWire ScreenCast(ashpd + pipewire-rs) | ✅ 编译通过;实时截屏需真机 |
| `backends::uinput` (`uinput`) | `InputSink` via uinput 内核虚拟设备(evdev);键码映射表全测 | ✅ 编译通过(Linux);实时注入需真机 |
| `backends::libei` (`libei`) | `InputSink` via libei/portal 模拟输入 | 🟡 预留脚手架(libei 未打包) |

trait 与 `NativeLayer` 一一对应(`CaptureSource::capture()`+`to_png` → `captureScreen()`;`InputSink` → `perform*()`)。**下一轮**:加 napi-rs 绑定 crate,把一个 `CaptureSource` + `InputSink` 组合成 `NativeLayer` 交给 `DesktopPlatform`,TS 侧零改动即接通。grounding(AT-SPI)不进此层。

> 本容器无头(那个 Wayland socket 是 VS Code 自身渲染),所以原生路径只能**编译 + 纯逻辑单测**,实时截屏/注入验证在真实 Wayland 主机做。

## 还没有 walker 的位置

graph walker（按 node 动态注入高层操作 / 已知边直行）**尚未实现**：loop 里 `tools` 硬编码为 `TOOL_DEFS`，无 node / walker 概念，每步无状态地重新感知。要做 M1，loop 契约至少要长出工具注入钩子或 walker 接管 act（见 [decisions.md](./decisions.md) D8）。当前 `Walker` / `GraphMap` 是 `@vrover/scout` 里的空占位。
