/**
 * @vrover/tools — the agent's tool surface (mark-based actions) + the executor that resolves a
 * tool call to a Platform primitive. Schemas are Zod → JSON Schema for the model.
 */
export { TOOL_DEFS } from './definitions.js';
export { dispatch } from './executor.js';
export type { DispatchResult } from './executor.js';
