/**
 * @vrover/llm — provider-agnostic LLM protocol types + provider adapters + runtime config.
 *
 * Each provider is a sibling module exporting a {@link CompleteFn} (the single LLM exit point,
 * dependency-injected into the agent loop); the types are a thin mirror of the wire format so
 * nothing else imports the SDK. `loadConfig` (env-driven runtime config) lives here — it's the
 * only consumer.
 */
export { complete } from './anthropic.js';
export { deepseekComplete } from './deepseek.js';
export { loadConfig } from './config.js';
export type { Config } from './config.js';
export type {
  Role,
  ContentBlock,
  Message,
  ToolDef,
  LLMResponse,
  CompleteRequest,
  CompleteFn,
} from './types.js';
