# 06 · Web UI / 传输协议 / 中断闭环 / 可视化 / Replay

> 映射 VRover：`apps/visual_rover_web`（koa serve API + React19 SPA，dev 用 Vite 中间件；刚从 Vue3 迁来，route 走 GLM provider）+ scout devtools（`apps/visual_scout_devtools`，Vue3 + Vite + HTTP/SSE）+ **`Agent.exec` / `Agent.pause`**。
>
> 对照对象：`multimodal/tarko/agent-ui`（React18 + Jotai SPA）+ `agent-ui-builder`（生成可分享 replay HTML）+ `agent-server`（SSE 流式 + abort）+ `apps/ui-tars`（Electron 屏幕标记覆盖层）。

---

## TODO 6.1 — SSE 流式传输协议（chat ↔ backend） `[传输/流式]` · **P0**

- **是什么**：前端 POST `/api/v1/sessions/query/stream` → 后端 `Content-Type: text/event-stream`，按 `data: ${JSON.stringify(event)}\n\n` 推事件；前端 `ReadableStream.getReader()` + `TextDecoder` 解析（兼容 `\r\n\r\n`/`\n\n`/`\r\r` 分隔），逐事件回调。
- **VRover 现状**：scout devtools **已用 SSE**（`/:id/stream`），但 `visual_rover_web` 的 chat↔backend 传输待确认/统一。
- **待办**：`visual_rover_web` 统一用 SSE 把 `Agent.exec` 的流式事件喂给 chat UI（与 [02](./02-agent-loop-exec-goto-pause.md) TODO 2.2 流式出口、[04](./04-llm-provider-streaming-mcp.md) TODO 4.1 流式签名串起来）。
- **参考出处**：
  - 后端：`UI-TARS-desktop/multimodal/tarko/agent-server/src/api/controllers/queries.ts`（SSE `res.write`，约 L101–156）
  - 前端：`UI-TARS-desktop/multimodal/tarko/agent-ui/src/common/services/apiService.ts#sendStreamingQuery`（reader 解析，约 L332–360）

## TODO 6.2 — 统一 `abort` 接口闭环（`Agent.pause`/停止的 UI 范本） `[中断]` · **P0**

- **是什么**：`IAgent.abort(): boolean` 是统一中断入口；session 调 `agent.abort()` 后经 EventStreamBridge emit `'aborted'`；UI 的 `abortQueryAction` 调 POST `/abort`，**成功后立即置 `isProcessing=false`**（不等下一帧，防用户误操作）。
- **VRover 缺口**：`Agent.pause`/停止的 UI 闭环未做。
- **待办**：web 加「停止」按钮 → POST 后端 → `Agent.abort()`（见 [02](./02-agent-loop-exec-goto-pause.md) TODO 2.1 的 `ExecutionController`）→ **UI 立即更新处理状态**。
- **参考出处**：
  - 接口：`UI-TARS-desktop/multimodal/tarko/agent-interface/src/agent.ts#abort`（约 L72–76）
  - session：`UI-TARS-desktop/multimodal/tarko/agent-server/src/core/AgentSession.ts#abortQuery`（约 L490–505）
  - UI：`UI-TARS-desktop/multimodal/tarko/agent-ui/src/common/state/actions/sessionActions.ts#abortQueryAction`（约 L411–433）

## TODO 6.3 — session-keyed 状态隔离（多会话切换不串味） `[会话管理]` · **P1**

- **是什么**：用 `sessionProcessingStatesAtom: Record<sessionId, boolean>` 按 sessionId 存处理状态；`isProcessingAtom` 是取当前 active session 状态的 derived atom；由 SSE `system` 事件 `processing_start`/`processing_end` 驱动。
- **VRover 缺口**：`visual_rover_web` 若要支持多会话切换，需按 sessionId 隔离状态。
- **待办**：chat UI 的处理状态/消息流按 sessionId 分桶，避免会话间污染。
- **参考出处**：`UI-TARS-desktop/multimodal/tarko/agent-ui/src/common/state/atoms/ui.ts#sessionProcessingStatesAtom` / `#isProcessingAtom`

## TODO 6.4 — 事件驱动 UI（统一分发 + 可回放） `[可视化]` · **P1**

- **是什么**：所有 SSE 事件经 `processEventAction` → `EventHandlerRegistry.findAllHandlers(event)` 分发给多个 handler（消息/工具调用/截图各一个）；**同一套 handler 既能处理实时事件，也能处理历史回放**。
- **VRover 缺口**：若 VRover 想在 UI 实时渲染每步的截图、点击 mark、thought，建议事件驱动而非在组件里直接处理响应。
- **参考出处**：`UI-TARS-desktop/multimodal/tarko/agent-ui/src/common/state/actions/eventProcessors/index.ts#processEventAction`

## TODO 6.5 — 消息分组（区分每个 `agent.exec()` 响应周期） `[可视化]` · **P2**

- **是什么**：`createMessageGroups` 把消息按逻辑分组——user 消息开新组，assistant 按 `messageId` 区分不同响应周期；支持流式消息去重/合并。
- **VRover 映射**：回合制 `exec` 下，给每次 exec 生成唯一 `messageId`，UI 据此分组渲染「这一轮的 thought + 动作 + 截图」。
- **参考出处**：`UI-TARS-desktop/multimodal/tarko/agent-ui/src/common/state/atoms/message.ts#createMessageGroups`

## TODO 6.6 — 屏幕标记覆盖层（真机演示 / debug 高亮 mark） `[可视化]` · **P2**

- **是什么**：`ScreenMarker` 单例在模型预测后建透明 `BrowserWindow`（`alwaysOnTop/transparent/focusable:false/type:'panel'`）叠 SVG（带编号红框 + 文本），5s 自动关；标记坐标由 `setOfMarksOverlays` 生成（含 scaleFactor 转换）；另有水流动画反馈。
- **VRover 映射**：桌面端真机跑时，把模型「即将操作的 mark」高亮在屏幕上，便于演示/debug（VRover 今天只有终端输出）。
- **参考出处**：
  - `UI-TARS-desktop/apps/ui-tars/src/main/window/ScreenMarker.ts#showPredictionMarker` / `#showScreenWaterFlow`
  - `UI-TARS-desktop/apps/ui-tars/src/main/shared/setOfMarks.ts`（mark 坐标计算）

## TODO 6.7 — 运行时设置 API（session 级模型/参数） `[设置]` · **P2**

- **是什么**：`/api/v1/runtime-settings` 返回 `{schema, currentValues}`，前端按 schema 动态渲染配置项（toggle/remove），更新同步进 session metadata。
- **VRover 映射**：`visual_rover_web` 的模型选择 / loop 参数 / maxSteps 等可做 session 级配置（不同会话用不同模型）。
- **参考出处**：`UI-TARS-desktop/multimodal/tarko/agent-ui/src/common/services/apiService.ts#getSessionRuntimeSettings` / `#updateSessionRuntimeSettings`

## TODO 6.8 — Agent 执行报告 / replay HTML 分享 `[replay]` · **P2**

- **是什么**：`AgentUIBuilder.dump()` 把 `events + sessionInfo + 版本 + uiConfig` 注入 HTML 模板的 `window.AGENT_*` 全局，生成**独立可分享的 replay HTML**（`AGENT_REPLAY_MODE`）；支持本地 staticPath 或远程 URL 加载 UI。
- **VRover 映射**：把一次 agent 执行（截图/动作/thought/结果）导出成单 HTML，便于演示、debug、bug 报告。
- **参考出处**：`UI-TARS-desktop/multimodal/tarko/agent-ui-builder/src/builder.ts#dump`
