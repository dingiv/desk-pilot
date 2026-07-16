import type { CompleteFn } from '@vrover/llm';
import { createOpenAICompatible, type OpenAICompatibleOptions } from './openai-compatible.js';

const GLM_DEFAULT_BASE = 'https://open.bigmodel.cn/api/paas/v4';
const GLM_DEFAULT_MODEL = 'GLM-5V-Turbo';

/**
 * GLM via Zhipu's native OpenAI-compatible endpoint (`…/api/paas/v4`). Reads
 * `GLM_API_KEY` / `GLM_BASE_URL` / `GLM_VISION_MODEL` from the environment when the factory is
 * called (lazy — importing the package never requires a key). Pass overrides for tests or a
 * known-config caller.
 */
export function createGlm(opts: Partial<OpenAICompatibleOptions> = {}): CompleteFn {
  const opt = {
    baseUrl: glmBaseUrl(),
    apiKey: process.env.GLM_API_KEY ?? '',
    model: process.env.GLM_VISION_MODEL ?? GLM_DEFAULT_MODEL,
    ...opts,
  };
  if (!opt.apiKey) {
    throw new Error(
      'GLM_API_KEY is not set. Add it to vrover.conf (llm.glm.apiKey) or set the GLM_API_KEY env var.',
    );
  }
  return createOpenAICompatible(opt);
}

/** Tolerate a `GLM_BASE_URL` that omits the `/v4` version segment (a common mis-set). */
function glmBaseUrl(): string {
  const raw = process.env.GLM_BASE_URL;
  if (!raw) return GLM_DEFAULT_BASE;
  const stripped = raw.replace(/\/+$/, '');
  return /\/v\d+$/.test(stripped) ? stripped : `${stripped}/v4`;
}
