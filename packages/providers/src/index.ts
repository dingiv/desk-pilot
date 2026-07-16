/**
 * @vrover/providers — the second `CompleteFn` implementation, a sibling to `@vrover/llm`'s
 * Anthropic adapter. One OpenAI-compatible core (`createOpenAICompatible`) covers GLM-native,
 * OpenAI, vLLM, llama.cpp, LM Studio and Ollama; `createGlm` / `createVllm` are thin presets,
 * and `createProviderFromEnv` picks one from the environment.
 *
 * Each factory returns a `CompleteFn` the agent loop takes as a dependency — the loop never
 * knows which provider (or wire format) it's running.
 */
export { createOpenAICompatible } from './openai-compatible.js';
export type { OpenAICompatibleOptions } from './openai-compatible.js';
export { createGlm } from './glm.js';
export { createVllm } from './vllm.js';
export { createProviderFromEnv } from './config.js';
