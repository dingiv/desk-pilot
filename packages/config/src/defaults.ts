import type { VroverConfig } from './types.js';

/** Built-in defaults — the lowest-priority layer. */
export const DEFAULTS: VroverConfig = {
  llm: {
    provider: 'glm',
    anthropic: {
      apiKey: '',
      model: 'claude-opus-4-8',
      effort: 'medium',
      maxTokens: 16000,
    },
    glm: {
      apiKey: '',
      baseUrl: 'https://open.bigmodel.cn/api/paas/v4',
      visionModel: 'GLM-5V-Turbo',
    },
    openai: {
      apiKey: '',
      baseUrl: 'https://api.openai.com/v1',
      model: 'gpt-4o',
    },
    vllm: {
      baseUrl: 'http://localhost:8000/v1',
      apiKey: '',
      model: 'Qwen2.5-VL-7B-Instruct',
    },
    custom: {
      baseUrl: '',
      apiKey: '',
      model: '',
    },
    deepseek: {
      apiKey: '',
      baseUrl: 'https://api.deepseek.com/anthropic',
      model: 'deepseek-v4-pro',
    },
  },
  scout: {
    host: '127.0.0.1',
    port: 7878,
  },
  agent: {
    maxSteps: 15,
    yoloPath: 'weights/icon_detect.onnx',
    boxThreshold: 0.05,
    iouThreshold: 0.1,
    debug: false,
    captureTimeoutMs: 30000,
    keepScreenshots: 2,
    contextWindow: 4,
  },
};
