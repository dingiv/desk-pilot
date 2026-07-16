import type { CompleteFn } from '@vrover/llm';
import { loadConfig as loadVroverConfig } from '@vrover/config';
import type { VroverConfig } from '@vrover/config';
import { createOpenAICompatible } from './openai-compatible.js';
import { createGlm } from './glm.js';
import { createVllm } from './vllm.js';

/**
 * Pick a provider from the unified config (vrover.conf + env).
 *
 * Reads lazily on call, so importing this package never requires a key.
 */
export function createProviderFromEnv(): CompleteFn {
  return createProviderFromConfig(loadVroverConfig());
}

/**
 * Same as {@link createProviderFromEnv} but accepts an explicit config object
 * (useful for tests or for passing CLI-overridden config).
 */
export function createProviderFromConfig(cfg: VroverConfig): CompleteFn {
  switch (cfg.llm.provider) {
    case 'openai':
      return createOpenAICompatible({
        baseUrl: cfg.llm.openai.baseUrl.replace(/\/+$/, ''),
        apiKey: cfg.llm.openai.apiKey,
        model: cfg.llm.openai.model,
      });
    case 'vllm':
      return createVllm({
        baseUrl: cfg.llm.vllm.baseUrl,
        apiKey: cfg.llm.vllm.apiKey,
        model: cfg.llm.vllm.model,
      });
    case 'custom':
      return createOpenAICompatible({
        baseUrl: cfg.llm.custom.baseUrl.replace(/\/+$/, ''),
        apiKey: cfg.llm.custom.apiKey,
        model: cfg.llm.custom.model,
      });
    case 'anthropic':
      // Anthropic is handled by @vrover/llm's complete() directly,
      // not through the providers layer. Fall back to GLM.
      return createGlm({
        apiKey: cfg.llm.glm.apiKey,
        baseUrl: cfg.llm.glm.baseUrl,
        model: cfg.llm.glm.visionModel,
      });
    case 'glm':
    default:
      return createGlm({
        apiKey: cfg.llm.glm.apiKey,
        baseUrl: cfg.llm.glm.baseUrl,
        model: cfg.llm.glm.visionModel,
      });
  }
}
