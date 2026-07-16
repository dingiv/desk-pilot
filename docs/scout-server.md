# Visual Scout — 独立 TCP server（自定义二进制协议 + 会话）

> 本文记录**已落地**的 Scout server 形态（代码为准）。组件边界见 [decisions.md](./decisions.md) D10；长期构想见 [design.md](./design.md)；现状架构见 [architecture.md](./architecture.md)。
> 状态：Scout 已落地为独立 **TCP server**，对外说一种**自定义二进制协议**；每个客户端连接经握手建立一个 **session**，session 拥有自己的操作终端（一个 `Platform` = 截屏器 + 一对键鼠）并预留 graph walker 状态。暴露 UI 操作 + grounding（D10 ①④）。

## 一句话

Visual Scout 是一个**独立进程**，把 GUI 目标的 **UI 操作 + grounding** 通过**自定义 TCP 协议**暴露给 VRover 大脑。大脑用 `RemotePlatform`（TCP client）驱动它——现有 `runAgent` / SoM / tools **一行不改**，只是把 `MockPlatform` 换成 `RemotePlatform`。

连接不是「连上就能用」：客户端必须先发一个 `HAND_SHAKE` 帧表明身份与需求，server 据此为它**铸造一个 session**（含独立的操作终端），回 `HAND_SHAKE_ACK`（带 `sessionId`），之后双方才按请求/响应通信。

```
VRover（大脑）                                                Visual Scout server（独立进程）
  runAgent ── RemotePlatform ────── TCP（自定义二进制协议）──────▶ net.createServer
              (@vrover/agent: RemotePlatform)                     │  server 级状态：session 注册表 + 共享 GraphMap（占位）
                                                                  │  每条连接：
   ① HAND_SHAKE {client, backend?}  ───────────────────────────▶▶│     backendFactory(req) → new Session(id, backend)
   ② HAND_SHAKE_ACK {sessionId,version,backend} ◀────────────────│     （session 持有 walker 占位）
   ③ REQUEST/RESULT/BLOB ◀────────────▶ 按 method 路由到 session.backend │  socket 关闭 → session 销毁
                                                                  │     GroundingSource（④ 缝，@vrover/scout/grounding）
```

关键：`Platform` 接口（`@vrover/platform`）是后端替换缝；`MultiScreenPlatform` / `DesktopPlatform` 都实现它；`RemotePlatform`（`@vrover/agent`）是大脑侧 TCP client。Server 用 `backendFactory: (req) => Platform` **每个会话铸造一个新后端**，客户端之间完全隔离。

## 状态：server 级 vs session 级

| 层级 | 归属 | 内容 |
|---|---|---|
| **server 级**（`server.ts`） | 整个进程 | session 注册表（`Map<sessionId, Session>`）、共享 `GraphMap`（一个应用一份；占位）、host/port/log |
| **session 级**（`session.ts`） | 每条连接 | `id`、`backend: Platform`（操作终端＝截屏器＋键鼠）、`grounding`、`walker`（占位） |

> 对应 D10：**graph map 是知识**（server 级、跨连接共享）；**walker 是会话状态**（session 级）。N 个连接 ⇒ N 个 session/walker ⇒ 共享 1 份 GraphMap。当前 `Walker` / `GraphMap` 都是**空占位**，等 D1（节点身份）/D2（DSL）定了再填逻辑。

## 协议（`packages/scout-protocol/`）

二进制帧。**12 字节大端头 + payload**：

| offset | 长度 | 字段 | 值 |
|---|---|---|---|
| 0 | 2 | magic | `0x53 0x43`（`'SC'`） |
| 2 | 1 | version | `1` |
| 3 | 1 | type | 见下表 |
| 4 | 4 | id | u32 BE，请求↔响应关联；握手/主动消息用 `0` |
| 8 | 4 | length | u32 BE，payload 字节数（不含头） |

控制消息（握手/请求/结果/错误）的 payload 是 UTF-8 JSON；截图结果走 raw BLOB（`[u32 width][u32 height][png]`，**不** base64）。

**type 取值：**

| type | 名 | 方向 | payload |
|---|---|---|---|
| `0x01` | `HAND_SHAKE` | C→S | JSON `HandshakeRequest`（id=0） |
| `0x02` | `HAND_SHAKE_ACK` | S→C | JSON `HandshakeAck`（id=0） |
| `0x03` | `ERROR` | S→C | JSON `{ error }`（id＝出错请求，或 0） |
| `0x10` | `REQUEST` | C→S | JSON `Request`（id＝请求 id） |
| `0x11` | `RESULT` | S→C | JSON 结果（id 对应请求） |
| `0x12` | `BLOB` | S→C | 二进制（id 对应请求） |

**`REQUEST` 的 `method`（与 `Platform` 原语一一对应）：**

| method | 请求体 | 响应 |
|---|---|---|
| `capture` | — | `BLOB`：`[u32 width][u32 height][png]` |
| `elements` | — | `RESULT` `{ elements: UiElement[] }` |
| `click` | `{ x, y }` | `RESULT` `{ ok: true }` |
| `type` | `{ text }` | `RESULT` `{ ok: true }` |
| `scroll` | `{ x, y, direction }` | `RESULT` `{ ok: true }` |
| `keypress` | `{ keys }` | `RESULT` `{ ok: true }` |

任何失败 → `ERROR { error }`（id 对应请求；握手失败 id=0 且 server 关连接）。`UiElement` 原样过线。

> `protocol.ts` 只管**搬字节**（帧编解码 + `FrameDecoder` 增量重组跨分片的帧）；`api.ts` 是**应用消息契约**（`Request`/`Handshake*` 形状与校验）。两者分层，server 与 client 共享，单一来源不漂移。

## 握手与生命周期

```
connect TCP
  └─ C→S  HAND_SHAKE   { client?, backend? }            # 首帧必须是其；否则 ERROR(id=0)+close
     S→C  HAND_SHAKE_ACK { sessionId, version, backend } # backendFactory(req) 铸造 session 后回
        ┌─ C→S REQUEST {method, ...}  (id=n)
        └─ S→C RESULT/BLOB            (id=n)   ── 失败则 ERROR (id=n)
  socket close → session 从注册表移除、backend 尽力 dispose
```

握手是异步的（`backendFactory` 可返回 Promise）；客户端 `RemotePlatform.ready` 在收到 ACK 时 resolve。多个并发请求按 `id` 关联，互不串。

## 怎么跑

```bash
pnpm install
pnpm --filter @vrover/scout example   # 起 server（无需 API key），监听 SCOUT_HOST:SCOUT_PORT（默认 127.0.0.1:7878）
```

> 注意：这是 **TCP** server，**不能 curl**。`pnpm test`（无需 key）即端到端验证：握手 / 截图 / 元素 / 登录 / 会话隔离 / 错误。

```bash
# 端到端：先用 @vrover/scout 起 server，再用 CLI 经 RemotePlatform 驱动它（大脑需 key）
cp .env.example .env             # 填 ANTHROPIC_API_KEY（大脑需要；server 不需要）
pnpm --filter @vrover/scout example                                              # 终端 1：起 server
pnpm --filter @vrover/visual-rover-cli start -- --platform remote --task "..."   # 终端 2：大脑驱动
```

host/port 由 `SCOUT_HOST` / `SCOUT_PORT` 控制（server 直接读环境变量，**不**走 `loadConfig()`，所以无 API key 也能跑）。

## 模块（pnpm workspace）

```
packages/scout-protocol/   线协议（传输层单一契约；leaf，被 client 与 server 共享）
  protocol.ts   二进制帧（头/magic/type/FrameDecoder/BLOB 编解码）
  api.ts        应用消息形状（Request method/Handshake*/Result/Error）+ 校验/编解码
  types.ts      UiElement / Bounds / CaptureResult（client 唯一依赖的类型来源）
packages/scout-client/     ScoutClient SDK（仅依赖 scout-protocol，面向第三方开发人员）
  scout-client.ts  connect/capture/elements/click/type/scroll/keypress/close
packages/scout/            Visual Scout TCP server
  server.ts     startScoutServer（net.createServer，握手→建 session→路由 REQUEST）
  session.ts    Session（session 级状态：backend + grounding + walker 占位）
  grounding.ts  PlatformGroundingSource（④ 缝，tier-1 直通 backend.getElements）
  walker.ts / graph-map.ts  占位（待 D1/D2）
packages/agent/            runAgent + RemotePlatform（大脑侧 TCP client；唯一消费 scout-client）
packages/platform/         MultiScreenPlatform（backendFactory 默认产出）/ MockPlatform / DesktopPlatform stub（Rust 缝）
```

## Rust 缝（预留，本轮不实现）

「关键高性能部分用 Rust」落在 `@vrover/platform`（`desktop.ts`）：`NativeLayer` 是未来 napi-rs 模块要满足的 TS 契约（xcap 截图 / enigo 键鼠 / AT-SPI 无障碍树），`DesktopPlatform` 把这些原语组装成 `Platform`。本轮没有 Rust 构建；未提供 `NativeLayer` 时 `DesktopPlatform` 每个方法抛清晰错误。等 napi-rs 落地，实现 `NativeLayer` 即可，`backendFactory` 可按握手 `backend` 字段返回它。

## 测试（无需 API key）

- `test/scout-protocol.test.ts` — 帧编解码 / 跨分片重组 / BLOB 布局 / 坏帧拒绝（纯字节，无 socket）。
- `test/scout-server.test.ts` — 握手建会话分配 `sessionId`；非握手首帧 → `ERROR(id=0)`；坏请求 → 同 `id` 的 `ERROR`；`sessionCount`；**两连接后端隔离**（per-session factory 证明）。
- `test/remote-platform.test.ts` — `RemotePlatform` 经 TCP：health / 截图解码为 Buffer / 登录流（对照 in-process mock）。
- `test/scout-loop.test.ts` — **端到端组件拆分**：未改的 `runAgent` + `RemotePlatform` + 脚本假 LLM，经 TCP 把后端登录跑通。

## 不在本轮（叠在 Scout 之上的下一层）

- **graph map + walker（D10 ②③）**——填 `Walker` / `GraphMap`：建应用图、`go_back`、按 node 动态注入高层操作（依赖 D1/D2 定稿）。
- 真 CV/OCR grounding（D11 tier 2/3）——插进 `GroundingSource`。
- 真 Rust 原生后端（xcap/enigo/AT-SPI）——填 `NativeLayer`/`DesktopPlatform`（design.md M5）。
- keepalive/ping、TLS/鉴权、断线重连恢复。
