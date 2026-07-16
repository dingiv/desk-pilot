# 04 · LLM Provider 抽象 / 流式签名 / ToolCallEngine / MCP

> 映射 VRover：`@vrover/llm`（`CompleteFn = (req) => Promise<LLMResponse>` 单一注入式出口，今天**非流式**，Anthropic 适配器 + GLM 经 Anthropic 兼容代理；`loadConfig` 读分层 `vrover.conf`）+ `@vrover/tools`（`TOOL_DEFS` 硬编码 + 内置 `dispatch`，无外部工具生态）。
>
> 对照对象：`multimodal/tarko/llm-client`（基于 token.js 的多厂商统一接口）+ `model-provider`（模型解析/拦截器）+ `tarko/agent`（`ToolCallEngine`）+ `tarko/mcp-agent` + `packages/agent-infra/mcp-*`（MCP 工具协议）。

---

## TODO 4.1 — 流式统一签名 `AsyncIterable<Chunk>`：`handleResponseChunk` 的范本 `[流式]` · **P0**

- **是什么**：tarko llm-client（token.js）把流式响应统一成 `StreamCompletionResponse = AsyncIterable<CompletionResponseChunk>`；`create()` 既能返回完整 `CompletionResponse` 也能返回该异步可迭代对象；Anthropic handler 的 `createCompletionResponseStreaming` 把 `MessageStream` 转成统一的 OpenAI 兼容 chunk 流。
- **VRover 缺口**：`CompleteFn` 非流式；`handleResponseChunk` 空壳。
- **待办**：给 `@vrover/llm` 加**流式变体** `StreamCompleteFn = (req) => AsyncIterable<LLMChunk>`，与 `CompleteFn` 并存（或 `CompleteFn` 返回值可流可整）；Anthropic adapter 接 SDK 流式；chunk 聚合成 `LLMResponse` 供非流式路径复用。**这条是 `Agent.exec` 流式出口（见 [02](./02-agent-loop-exec-goto-pause.md) TODO 2.2）的前置**。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/llm-client/src/userTypes/index.ts#StreamCompletionResponse`（`AsyncIterable<CompletionResponseChunk>`）
  - `UI-TARS-desktop/multimodal/tarko/llm-client/src/chat/index.ts#create`（流式/非流式同入口）
  - `UI-TARS-desktop/multimodal/tarko/llm-client/src/handlers/anthropic.ts#createCompletionResponseStreaming`（Anthropic `MessageStream` → chunk 流）

## TODO 4.2 — Provider 能力矩阵 + 动态注册模型 `[provider抽象/配置]` · **P1**

- **是什么**：`BaseHandler` 用一组能力字段（`supportsStreaming/supportsToolCalls/supportsImages/supportsJSON`，可按模型列表或 `true`）声明能力；`extendModelList(provider, name, featureSupport)` **运行时动态注册新模型**及其能力，无需改核心代码。
- **VRover 缺口**：仅 Anthropic + GLM（经兼容代理），无显式能力矩阵；加新模型/新 provider 要改适配器。
- **待办**：引入能力矩阵（校验「该模型是否支持 tool_calls / vision / 流式」）；GLM 等通过类似 `extendModelList` 注册。与 `@vrover/config` 的 provider 配置打通。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/llm-client/src/handlers/base.ts#BaseHandler`
  - `UI-TARS-desktop/multimodal/tarko/llm-client/src/index.ts#extendModelList`

## TODO 4.3 — `resolveModel` 分层优先级 + 请求拦截器 `[配置]` · **P1**

- **是什么**：`resolveModel()` 按 **run options > agent config > defaults** 解析模型，并对扩展 provider（ollama/volcengine 等）自动降级到 `openai-compatible`；`createLLMClient` 接受 `requestInterceptor`，可在发请求前改 payload/注入 header（trace/debug）。
- **VRover 现状**：`loadConfig` 分层 `vrover.conf` 已有，但无 run-time override、无请求拦截器。
- **待办**：配置解析支持 run-time override（CLI 参数覆盖 `vrover.conf`）；在 `CompleteFn` 前加可选 `requestInterceptor`（注入 trace header / 改 payload），便于调试与未来 A/B。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/model-provider/src/model-resolver.ts#resolveModel`
  - `UI-TARS-desktop/multimodal/tarko/model-provider/src/llm-client.ts#createLLMClient`

## TODO 4.4 — `ToolCallEngine` 可插拔（Native / StructuredOutputs / PromptEngineering） `[tool-calling]` · **P2**

- **是什么**：抽象 `ToolCallEngine` 基类，三种实现：原生 function calling / 强制 JSON schema 输出 / prompt-engineering 解析；运行时按配置选；并含流式增量解析（`processStreamingChunk` + buffer/state）。
- **VRover 现状**：用 Anthropic 原生 tool_calls，单一模式。
- **待办**：**暂不引入**（VRover 默认 native 即可）。仅在未来要兼容**不支持 tool_calls 的模型**时，加一个 `PromptEngineeringToolCallEngine`；其流式增量解析（buffer + state）可给 VRover 流式工具调用解析当参考。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/agent/src/tool-call-engine/NativeToolCallEngine.ts`
  - `UI-TARS-desktop/multimodal/tarko/agent/src/tool-call-engine/index.ts`

## TODO 4.5 — 引入 MCP 工具协议（让用户挂自定义工具 server） `[MCP/工具注册]` · **P2**

- **是什么**：`MCPAgent` 初始化时连 MCP servers、把每个 MCP 工具注册成 agent 工具（`[serverName] desc` + JSON Schema 参数 + `callTool`）；`MCPClient` 支持三种传输：**stdio**（子进程）、**SSE**（URL+EventSource）、**Streamable HTTP**（URL+fetch），还有 in-memory pair。
- **VRover 缺口**：工具是硬编码 `TOOL_DEFS` + 内置 `dispatch`，**无外部工具生态**——用户不能挂自己的工具。
- **待办（评估性）**：若 VRover 要支持「用户自定义工具」，引入 MCP（`@modelcontextprotocol/sdk`），优先 stdio（本地 server）+ SSE（远程）；把 MCP 工具适配进 `@vrover/tools` 的 `Tool` 形态（注意 MCP 用 JSON Schema 7，VRover 工具定义需兼容）。**当前 SoM 工具集够用，这是远期扩展**。
- **参考出处**：
  - `UI-TARS-desktop/multimodal/tarko/mcp-agent/src/mcp-agent.ts#initialize`
  - `UI-TARS-desktop/packages/agent-infra/mcp-client/src/index.ts#activate`（按 `url`/`command`/`mcpServer` 选 transport）
  - `UI-TARS-desktop/packages/agent-infra/mcp-shared/src/client/types.ts`（server 配置类型）

---

## 已评估·低优先

- **独立 `LLMRequester`**（脱离 agent 直接发 LLM 请求、从 JSONL 批量发）：可做调试/benchmark 小工具，P2。
  - 出处：`UI-TARS-desktop/multimodal/tarko/agent/src/utils/llm-requester.ts#LLMRequester`。
