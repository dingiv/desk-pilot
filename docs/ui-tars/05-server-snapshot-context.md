# 05 · Server 形态 / 会话存储恢复 / 快照录制回放 / 上下文工程

> 映射 VRover：`@vrover/scout`（**独立 TCP server** + 自定义二进制协议 + per-connection session；session 内存临时，断开即丢）+ **`Agent.goto`**（历史分支，净新设计）+ `packages/agent/src/context.ts#pruneForModel`（滑动窗口压缩 + 截图封顶）。
>
> 对照对象：`multimodal/tarko/agent-server`（Express HTTP server + 存储 + 会话恢复）+ `agent-snapshot`（**为测试**的录制/回放）+ `context-engineer`（`@file:/@dir:` 引用扩展 + 工作区打包）。
>
> ⚠️ **关键**：tarko **没有 goto / 历史分支**（穷举确认）。`agent-snapshot` 是录制/回放（每个快照=独立测试用例），**不能 fork**。所以 VRover 的 `goto` 自研，tarko 只提供「录制/恢复/校验」底座。

---

## TODO 5.1 — `StorageProvider` 多存储后端抽象（session 持久化） `[server形态/存储]` · **P2**

- **是什么**：统一 `StorageProvider` 接口（`createSession/updateSessionInfo/getSessionInfo/getAllSessions/deleteSession/saveEvent/getSessionEvents`），四实现 memory/file/sqlite/mongodb。session 元数据 + 事件可落盘。
- **VRover 缺口**：scout session 纯内存，进程重启/断线即丢，无多客户端共享历史。
- **待办（远期）**：给 scout session 加可选 `StorageProvider`（先 file/sqlite），支持重启恢复 + 断线重连续跑。VRover 当前单机桌面场景未必急需，P2。
- **参考出处**：`UI-TARS-desktop/multimodal/tarko/agent-server/src/storage/types.ts#StorageProvider`（及同目录 `Memory/File/SQLite/MongoDB` 实现）

## TODO 5.2 — `sessionRestoreMiddleware` 会话自动恢复 `[会话/恢复]` · **P1**

- **是什么**：请求进来若 session 不在内存但存储里有，中间件自动 `new AgentSession(...)` + `initialize({initialEvents: storedEvents})` 重建并重放历史事件。
- **VRover 缺口**：scout 无恢复能力。
- **待办**：配合 5.1，scout server 启动/断线后按 sessionId 从存储重建 session + 重放历史（事件流）。对 `Agent.goto` 也有用——「回到某历史点」≈「从快照恢复到该点」。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/agent-server/src/api/middleware/session-restore.ts#sessionRestoreMiddleware`
  - `UI-TARS-desktop/multimodal/tarko/agent-server/src/core/AgentSession.ts#initialize`（`initialEvents` 注入）

## TODO 5.3 — Hook 式快照录制（按 loop 分目录，`goto` 的底座） `[快照/录制]` · **P1**

- **是什么**：`AgentGenerateSnapshotHook` 通过 hook（`onLLMRequest/onLLMStreamingResponse/onEachAgentLoopStart`）拦截每步，按 `loop-N/` 分目录写 `llm-request.jsonl` / `llm-response.jsonl`（流式为多行 JSONL）/ `event-stream.jsonl` / `tool-calls.jsonl`。`countLoops` 数 loop 目录。
- **VRover 缺口**：无执行过程录制——debug 只能靠 `log`。
- **待办**：给 `Agent` 加**可选**录制 hook，每步（请求/响应/事件流/工具调用）落盘；VRover 的 turn 边界（带 image 的 observe 消息，`context.ts#groupTurns`）天然对应 `loop-N/`，**这就是 `goto(turnIndex)` 的切片点**——录制 = 可恢复 = 可分支。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/agent-snapshot/src/agent-generate-snapshot-hook.ts#onLLMRequest` / `#onLLMStreamingResponse`
  - `UI-TARS-desktop/multimodal/tarko/agent-snapshot/src/agent-snapshot.ts#countLoops`

## TODO 5.4 — 快照回放 + 三层校验（行为一致性 / 回归测试） `[快照/回放]` · **P2**

- **是什么**：`AgentReplaySnapshotHook` 造 **Mock LLM client**，从快照读预存响应按 loop 喂回；三层校验：LLM request 一致 / event-stream 一致 / tool-call 一致；支持 `updateSnapshots` 模式（不报错直接更新快照）。
- **VRover 映射**：`goto` 后可用快照校验「分支后行为是否符合预期」；也可做 agent 回归测试 / 检测行为漂移。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/agent-snapshot/src/agent-replay-snapshot-hook.ts#createMockLLMClient`
  - `UI-TARS-desktop/multimodal/tarko/agent-snapshot/src/snapshot-manager.ts#verifyRequestSnapshot` / `#verifyEventStreamSnapshot`

## TODO 5.5 — 透明包装器模式（无侵入给 Agent 加录制/回放） `[快照/架构]` · **P2**

- **是什么**：`AgentSnapshot` 包装原 `Agent`，用原型链继承 + 属性代理，对外 `run()` 签名不变，内部透明拦截所有 LLM 调用。
- **VRover 映射**：VRover 可用同样模式包装 `Agent`，无侵入地加录制/回放/trace，不污染核心循环。
- **参考出处**：`UI-TARS-desktop/multimodal/tarko/agent-snapshot/src/agent-snapshot.ts#constructor`

## TODO 5.6 — `@file:/@dir:` 上下文引用扩展 + 工作区打包 `[上下文]` · **P2**

- **是什么**：`ContextReferenceProcessor` 支持在 query 里写 `@file:path` / `@dir:path`，自动把文件内容/目录打包（`<file path>…</file>`）扩进 LLM 上下文（含 workspace 路径穿越安全检查）；`WorkspacePack` 配置化高效打包大目录（`maxFileSize/ignoreExtensions/ignoreDirs/maxDepth`）。
- **VRover 现状**：`pruneForModel` 只做历史压缩，无「把外部内容塞进上下文」能力。
- **待办（桌面 agent 场景）**：支持用户在任务里引用文件/目录/窗口列表等环境信息，自动扩进上下文——比 SoM 元素表更丰富的环境注入。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/context-engineer/src/node/context-reference-processor.ts#processContextReferences`
  - `UI-TARS-desktop/multimodal/tarko/context-engineer/src/node/workspace-pack.ts`

---

## 对照·不盲从

- **HTTP/JSON server（agent-server 用 Express）vs VRover scout TCP 二进制**：VRover 的二进制协议（PNG 直传、无 base64）**更省**，server 形态**不照搬** tarko 的 HTTP；只借鉴其**存储/恢复/事件过滤**思路。tarko 的 `shouldStoreEvent`（过滤 `*_streaming_*` 事件不落盘）可参考以减存储压力。
  - 出处：`UI-TARS-desktop/multimodal/tarko/agent-server/src/core/AgentSession.ts#shouldStoreEvent`。
- **`agent-server-next`（Hono 新一代）**：仅框架迁移，核心同 agent-server，**忽略**。
