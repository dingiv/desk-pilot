import { loadConfig as loadVroverConfig } from '@vrover/config';
import type { VroverConfig } from '@vrover/config';

// Re-export the unified config loader for downstream consumers.
export { loadVroverConfig };
export type { VroverConfig };

/** Runtime configuration, now sourced from vrover.conf + env vars. */
export interface Config {
  anthropicApiKey: string;
  model: string;
  effort: 'low' | 'medium' | 'high' | 'xhigh' | 'max';
  maxTokens: number;
  maxSteps: number;
  /** Where the Visual Scout server listens (brain/client side reads this). */
  scoutHost: string;
  scoutPort: number;
  /** DeepSeek (Anthropic-compatible endpoint). */
  deepseek: {
    apiKey: string;
    baseUrl: string;
    model: string;
  };
}

let cached: Config | undefined;

/** Load and cache config from the unified vrover.conf system + env. */
export function loadConfig(): Config {
  if (cached) return cached;
  const cfg = loadVroverConfig();
  cached = {
    anthropicApiKey: cfg.llm.anthropic.apiKey,
    model: cfg.llm.anthropic.model,
    effort: cfg.llm.anthropic.effort,
    maxTokens: cfg.llm.anthropic.maxTokens,
    maxSteps: cfg.agent.maxSteps,
    scoutHost: cfg.scout.host,
    scoutPort: cfg.scout.port,
    deepseek: {
      apiKey: cfg.llm.deepseek.apiKey,
      baseUrl: cfg.llm.deepseek.baseUrl,
      model: cfg.llm.deepseek.model,
    },
  };
  return cached;
}
