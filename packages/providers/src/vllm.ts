import type { CompleteFn } from '@vrover/llm';
import { createOpenAICompatible, type OpenAICompatibleOptions } from './openai-compatible.js';

const VLLM_DEFAULT_BASE = 'http://localhost:8000/v1';
// Must match the server's `--served-model-name`; override via VLLM_MODEL.
const VLLM_DEFAULT_MODEL = 'Qwen2.5-VL-7B-Instruct';

/**
 * A locally-deployed model served by vLLM (OpenAI-compatible `/v1`). No auth by default — set
 * `VLLM_API_KEY` only if the server was started with `--api-key`. The model must match the
 * server's `--served-model-name`, so override `VLLM_MODEL` to whatever you launched.
 */
export function createVllm(opts: Partial<OpenAICompatibleOptions> = {}): CompleteFn {
  return createOpenAICompatible({
    baseUrl: (process.env.VLLM_BASE_URL ?? VLLM_DEFAULT_BASE).replace(/\/+$/, ''),
    apiKey: process.env.VLLM_API_KEY ?? '',
    model: process.env.VLLM_MODEL ?? VLLM_DEFAULT_MODEL,
    ...opts,
  });
}
