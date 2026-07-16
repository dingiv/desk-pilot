# 01 · Platform / Operator 多后端抽象

> 映射 VRover：`@vrover/platform`（`Platform` 接口 + `MockPlatform`/`DesktopPlatform`/`RemotePlatform`）+ `crates/drivers`（PipeWire 截屏 + uinput/libei 输入）。
>
> 对照对象：`UI-TARS-desktop/multimodal/gui-agent/` 的 `operator-nutjs` / `operator-browser` / `operator-adb` / `operator-aio` + `shared`（吸收差异的共享层）。这是**与 VRover 重叠最大**的子系统，逐条对比。

VRover 现状：`Platform` 是**纯接口**，方法 `captureScreen/getElements/performClick/performType/performScroll/performKeypress` 全是**原子操作 + 像素坐标**；`Screenshot` 只有 `width/height/png`，无缩放因子；`DesktopPlatform` 是 Rust/napi 占位（未给 `NativeLayer` 时直接抛错）。

---

## TODO 1.1 — `ScreenContext`：把缩放因子（DPR / scaleX·Y）提到一等公民 `[屏幕上下文]` · **P1**

- **是什么**：tarko 用一个 `ScreenContext` 类型统一屏幕尺寸 + 缩放，各后端 `initialize()` 时算好缓存。HiDPI / Retina / 缩放设置都靠它正确换算坐标。
- **VRover 缺口**：`Screenshot` 只有 `width/height`，**无 scale**。`DesktopPlatform` 接 PipeWire 时若不处理 DPR，HiDPI 屏会点偏。`crates/drivers` 已能截屏但未暴露分辨率/DPR。
- **待办**：给 `@vrover/platform` 的 `Screenshot`（或新增 `ScreenContext`）加 `scaleX/scaleY`（或 `scaleFactor`）；`crates/drivers` PipeWire 侧把 `pixelDensity` 一并返回；TS 侧的坐标换算统一用它。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/base/operator.ts#ScreenContext`
  - `UI-TARS-desktop/multimodal/gui-agent/operator-nutjs/src/NutJSOperator.ts#initialize`（从 `screen.grab().pixelDensity` 算 scaleX/Y）
  - `UI-TARS-desktop/multimodal/gui-agent/operator-browser/src/browser-operator.ts#initialize`（`getDeviceScaleFactor`）
  - `UI-TARS-desktop/apps/ui-tars/src/main/utils/screen.ts#getScreenSize`（macOS `scaleFactor` 固定 1 的特例处理）

## TODO 1.2 — `Operator` 抽象基类：懒初始化 + 安全执行封装 `[接口契约]` · **P1**

- **是什么**：`Operator` 抽象类强制子类实现 `initialize/supportedActions/screenContext/screenshot/execute`，并提供 `do*` 安全包装：`doInitialize()`（并发安全、只跑一次的懒初始化）、`doScreenshot()/doExecute()`（自动 `ensureInitialized` + **catch 成 `{status:'failed', errorMessage}`** 而非抛崩）。
- **VRover 缺口**：`Platform` 是纯接口，**无懒初始化**（各实现自己管），**无错误封装**——`DesktopPlatform` 未实现时直接抛，错误处理散落。
- **待办**：把 `Platform` 升级为抽象基类（或加 `BasePlatform`），内置懒初始化 + `do*` 安全封装；让 `DesktopPlatform`/`RemotePlatform` 只实现核心方法。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/base/operator.ts#Operator`（`doInitialize/doScreenshot/doExecute`、`_initialized/_initPromise` 并发守卫）
  - `UI-TARS-desktop/multimodal/gui-agent/operator-nutjs/src/NutJSOperator.ts#NutJSOperator`

## TODO 1.3 — 类型化 Action + `ACTION_METADATA` 元数据注册 `[接口契约/工具描述]` · **P1**

- **是什么**：所有动作是泛型 `BaseAction<T,I>`（`type` + `inputs` + 可选 `meta.{toolHint,comment}`），并用一张 `ACTION_METADATA` 表登记每个动作的 `category`/`description` + 类型守卫 `isSupportedActionType`。
- **VRover 缺口**：`Action` 是紧凑 discriminated union（无泛型、无元数据）。元数据的好处是**能自动生成喂给 LLM 的工具描述 / action_space**，以及统一日志/调试。
- **待办**：给 `@vrover/tools` 的工具定义补一层**元数据**（category / 中文 description / 是否需要 mark），用于自动渲染 system prompt 的 action space 与 devtools 展示。注意 VRover 用 tool_calls，元数据主要服务于**提示词生成 + 可视化**，不必改成 tarko 的自由文本动作结构。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/types/actions.ts#BaseAction`
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/types/actions.ts#ACTION_METADATA`
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/types/actions.ts#isSupportedActionType`

## TODO 1.4 — `supportedActions()` 能力矩阵：各后端声明能力、不支持即显式抛错 `[多后端/降级]` · **P1**

- **是什么**：每个 operator 声明自己支持的动作集（桌面有 `hotkey`、安卓有 `press_home/swipe`、浏览器有 `navigate`）；执行时遇到不支持的 `default → throw`，而非默默全量实现。
- **VRover 缺口**：`Platform` 是**全量接口**——所有实现必须支持全部方法，否则运行时抛 `'not implemented'`。未来加 `AndroidPlatform`（有 `press_home`）/`BrowserPlatform`（有 `navigate`）时会很别扭。
- **待办**：给 `Platform` 加能力查询（`supportedActions(): Set<ActionType>` 或 `supportsAction(type)`）；loop/dispatch 在执行前校验能力，不支持时给模型回「该平台不支持此动作」的 tool_result（而非崩）。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/gui-agent/operator-nutjs/src/NutJSOperator.ts#supportedActions`
  - `UI-TARS-desktop/multimodal/gui-agent/operator-browser/src/browser-operator.ts#supportedActions`
  - `UI-TARS-desktop/multimodal/gui-agent/operator-adb/src/AdbOperator.ts#supportedActions`
  - `UI-TARS-desktop/multimodal/gui-agent/operator-nutjs/src/NutJSOperator.ts#singleActionExecutor`（`default: throw` 降级）

## TODO 1.5 — `Coordinates` 双重表示（raw 像素 + normalized 0–1）+ 共享转换工具 `[坐标]` · **P2**

- **是什么**：`Coordinates` 同时支持 `raw`（像素）与 `normalized`（0–1），`referenceSystem` 标参考系；共享层 `normalizeActionCoords` / operator 内 `calculateRealCoords` 负责互转，上层可用归一化坐标、后端自管分辨率。
- **VRover 现状**：`Platform` 仅像素坐标。
- **⚠️ 慎借**：VRover 用 **SoM mark 编号**引用元素，坐标只在内部「mark→bounds→center」用，**上层模型从不直接吐坐标**。所以归一化坐标对 VRover **价值有限**，仅在「未来要支持裸坐标 / 跨分辨率录制回放」时参考。优先级 P2。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/types/actions.ts#Coordinates`
  - `UI-TARS-desktop/multimodal/gui-agent/operator-nutjs/src/NutJSOperator.ts#calculateRealCoords`
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/utils/coordinateNormalizer.ts#normalizeActionCoords`

---

## 已评估·不借鉴（记录对照，免日后重复讨论）

- **批量执行接口**（`execute(actions: BaseAction[])`，operator 内部 for 循环）：tarko 把批量控制塞进 operator；VRover 的**原子操作 + 批量控制由 agent loop 负责**更干净（可重试/可插观察/可并行），**不借鉴**。
  - 出处：`UI-TARS-desktop/multimodal/gui-agent/shared/src/types/agents.ts#ExecuteParams`、`operator-nutjs/src/NutJSOperator.ts#execute`。
- **远程 operator 走 HTTP（`AIOHybridOperator`）**：VRover 的 `RemotePlatform` + scout **自定义二进制 TCP 协议（PNG 直传、无 base64）**已比 HTTP/JSON 更省，**不借鉴**。
  - 出处：`UI-TARS-desktop/multimodal/gui-agent/operator-aio/src/AIOHybridOperator.ts`。
- **浏览器 operator 的 UI 辅助**（高亮可点元素、水流动画）：调试辅助，VRover scout devtools 已有等价 Web 调试面，**不借鉴**。
  - 出处：`UI-TARS-desktop/multimodal/gui-agent/operator-browser/src/browser-operator.ts#screenshot`、`ui-helper.ts`。
