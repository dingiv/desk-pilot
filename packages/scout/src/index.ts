/**
 * @vrover/scout — the Visual Scout TCP server ("胖工具"). Exposes UI operations + grounding over
 * the custom binary protocol (`@vrover/scout-protocol`) with per-connection sessions; each
 * session owns its own operation terminal (a `Platform`) and a graph-walker placeholder.
 * The standalone client SDK lives in `@vrover/scout-client`; protocol types in
 * `@vrover/scout-protocol`.
 */
export { startScoutServer } from './server.js';
export type { ScoutServer, ScoutServerOptions } from './server.js';
export { startDevtoolsServer } from './devtools.js';
export type { DevtoolsServer, DevtoolsOptions, DevtoolsContext, DevtoolsConfig } from './devtools.js';
export { Session } from './session.js';
export { PlatformGroundingSource } from './grounding.js';
export { Walker } from './walker.js';
export { GraphMap } from './graph-map.js';
