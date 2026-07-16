/**
 * @vrover/scout-client — a standalone client SDK for the Visual Scout server.
 *
 * A thin JS interface over the Scout TCP protocol, for writing automation
 * scripts (the "人类 JS 脚本 API" from docs/design.md). Depends **only** on
 * `@vrover/scout-protocol` — no other project modules — so third-party
 * developers can install just this package. Internally, only the brain
 * (`@vrover/agent`, via `RemotePlatform`) consumes it.
 */
export { ScoutClient } from './scout-client.js';
export type { HandshakeRequest, HandshakeAck, CaptureResult, UiElement, Bounds } from '@vrover/scout-protocol';
