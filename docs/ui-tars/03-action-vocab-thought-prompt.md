# 03 · 动作词表 / 推理链 / 截图 smart_resize / 提示词

> 映射 VRover：`@vrover/tools`（`TOOL_DEFS` + `dispatch`）+ `packages/agent/src/prompts/`（system/step/nudge）+ 截图管线（`@vrover/platform` capture / `crates/drivers` PipeWire）。
>
> 对照对象：`UI-TARS/codes/ui_tars/`（Python 权威：`action_parser.py` + `prompt.py`）+ `UI-TARS-desktop/multimodal/gui-agent/action-parser/`（TS 移植）。
>
> ⚠️ **范式差异**：UI-TARS 走「自由文本动作 + 裸坐标」；VRover 走「tool_calls + SoM mark 编号」。所以 UI-TARS 的**解析器链、坐标系映射**对 VRover 价值低（README 结论 1）。要借鉴的是：**动作词表、Thought 推理链、完成/中断信号语义、截图 smart_resize**。

VRover 现状（按 `@vrover/tools` / `@vrover/platform` Action 联合类型）：动作约 `click / doubleClick / type(mark,text) / scroll(mark, up|down) / keypress(keys) / wait / done(summary)`；`SYSTEM_PROMPT` 只要求「每步恰好一个工具」，**未强制推理**。

---

## TODO 3.1 — 补全动作词表：`drag` / `scroll` 左右 / 右键·中键 `[动作词表]` · **P1**

- **是什么**：UI-TARS 动作空间（见 `prompt.py` + `ACTION_METADATA`）含：`click / left_double / right_single / hover`、`drag(start,end)`、`type`、`scroll(point, up|down|left|right)`、`hotkey(≤3 键)`、`wait`、`finished(content)`、`press/keydown/keyup`，移动端还有 `long_press/swipe/press_home/press_back/open_app`。
- **VRover 缺口**（对照 `@vrover/tools` 后确认）：缺 **`drag`（拖拽：起止 mark）**；`scroll` 只有 up/down（**缺 left/right**）；可能缺 **右键/中键点击**。
- **待办**：在 `@vrover/tools` 增补 `drag(startMark, endMark)`、`scroll` 加 `left|right`、`rightClick` 等；dispatch + `crates/drivers` uinput 后端实现 mouse-down/move/up。
- **参考出处**：
  - `UI-TARS/codes/ui_tars/prompt.py`（动作空间清单，约 L12–22）
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/types/actions.ts#ACTION_METADATA`（全集 + category）
  - `UI-TARS-desktop/multimodal/gui-agent/operator-nutjs/src/NutJSOperator.ts#singleActionExecutor`（每种动作的执行实现）

## TODO 3.2 — Thought 推理链：要求模型 act 前先推理 `[推理链]` · **P0**

- **是什么**：UI-TARS 强制模型在动作前输出 `Thought: ...`（UI-TARS-1.5 还支持 `Reflection/Action_Summary/Action` 三段式），parser 把推理抽进 `thought/reflection` 字段。论文论证推理链显著提升多步任务成功率（inference-time reasoning）。
- **VRover 缺口**：`SYSTEM_PROMPT` 只说「每步一个工具」，**未鼓励/强制推理**——模型可能直接吐 tool_call 不解释。
- **待办**：在 `prompts`（system + step）里要求模型**先写一段 Thought 再调工具**（VRover 走 tool_calls，可在同一 assistant turn 内先 text 后 tool_use，或把推理塞进 tool input 的 `reasoning` 字段）；devtools / web UI 把 thought 渲染出来。
- **参考出处**：
  - `UI-TARS/codes/ui_tars/action_parser.py`（`Thought:` / `Reflection:` 解析，约 L173–193）
  - `UI-TARS-desktop/multimodal/gui-agent/action-parser/src/actionParser.ts`（TS 版 Thought 提取，约 L185–205）
  - `UI-TARS/codes/ui_tars/prompt.py`（prompt 里 `Thought` 段的约束 + Note）

## TODO 3.3 — `call_user` 信号：agent 主动把控制权交回人 `[信号]` · **P1**

- **是什么**：UI-TARS 有 `call_user`（`ACTION_METADATA.category='system'`）——agent 卡住/需澄清时主动「请求人类介入」，而非硬试或误操作。`finished(content)` 则是正常完成返回总结。
- **VRover 缺口**：VRover 有 `done(summary)` ≈ `finished`，但**缺「求助/澄清」信号**。
- **待办**：加一个 `ask_user(question)` 工具（或复用 `Agent.exec` 回合制语义：agent 主动结束本轮、把问题抛回用户等下一条消息）。对**交互式 chat**（`Agent.exec`）尤其有用——agent 能「停下来问」而非一直自动跑。
- **参考出处**：
  - `UI-TARS/codes/ui_tars/prompt.py`（动作空间含 `call_user`/`finished`）
  - `UI-TARS-desktop/multimodal/gui-agent/shared/src/types/actions.ts#ACTION_METADATA`（`call_user`/`finished` 条目）

## TODO 3.4 — 截图 `smart_resize`：按 VLM 分辨率约束缩放 `[截图]` · **P1**

- **是什么**：为适配 VLM 输入分辨率限制，`smart_resize(height,width)` 保证图片宽高是 `IMAGE_FACTOR=28` 的倍数、像素数落在 `[MIN_PIXELS, MAX_PIXELS]`（约 100·28² ~ 16384·28²）且保持纵横比。TS 移植版用 `smartResizeFactors` 把模型输出的归一化坐标反算回屏幕。
- **VRover 缺口**：VRover 截图**直传 VLM**，未做分辨率约束（大屏截图可能超 VLM 上限或浪费 token）。**注意**：VRover 用 mark 编号，坐标反算那半对 VRover 无关；要借鉴的只是**截图送模型前的 smart_resize**。
- **待办**：在 capture 后（`@vrover/platform` 或 scout capture）对 PNG 做一次 `smart_resize` 再进 history；移植 Python `smart_resize`（纯算术）到 TS。
- **参考出处**：
  - `UI-TARS/codes/ui_tars/action_parser.py#smart_resize`（L115–143，权威实现）
  - `UI-TARS-desktop/multimodal/gui-agent/action-parser/src/actionParser.ts`（`smartResizeFactors` 用法，约 L267–297）

---

## 已评估·不借鉴（记录对照）

- **多格式容错解析器链**（`FormatParserChain`：XML/Omni/Unified-BC/BCComplex/Fallback 五种 parser 依次试）：为兼容不同模型输出的自由文本格式。VRover 用 **tool_calls 标准化**（provider 保证格式），**无需**这套容错链。
  - 出处：`UI-TARS-desktop/multimodal/gui-agent/action-parser/src/FomatParsers.ts#FormatParserChain`。
- **归一化坐标↔像素映射**：VRover 用 mark 编号天然绕开，**不借鉴**（见 [01](./01-platform-operator.md) TODO 1.5 的慎借说明）。
- **移动端动作**（`open_app/press_home/press_back/swipe`）：VRover 当前专注桌面，远期做 Android target 时再参考 `operator-adb`。
