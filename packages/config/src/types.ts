/** VRover unified configuration. */
export interface VroverConfig {
  llm: LlmSection;
  scout: ScoutSection;
  agent: AgentSection;
}

export interface LlmSection {
  /** Which provider to use. */
  provider: 'glm' | 'openai' | 'vllm' | 'custom' | 'anthropic' | 'deepseek';
  anthropic: AnthropicConfig;
  glm: GlmConfig;
  openai: OpenAiConfig;
  vllm: VllmConfig;
  custom: CustomConfig;
  deepseek: DeepSeekConfig;
}

/**
 * DeepSeek — reached via its Anthropic-compatible endpoint (`/anthropic/v1/messages`),
 * so it shares the Anthropic wire format but with its own base URL / model / key.
 */
export interface DeepSeekConfig {
  apiKey: string;
  baseUrl: string;
  model: string;
}

export interface AnthropicConfig {
  apiKey: string;
  model: string;
  effort: 'low' | 'medium' | 'high' | 'xhigh' | 'max';
  maxTokens: number;
}

export interface GlmConfig {
  apiKey: string;
  baseUrl: string;
  visionModel: string;
}

export interface OpenAiConfig {
  apiKey: string;
  baseUrl: string;
  model: string;
}

export interface VllmConfig {
  baseUrl: string;
  apiKey: string;
  model: string;
}

export interface CustomConfig {
  baseUrl: string;
  apiKey: string;
  model: string;
}

export interface ScoutSection {
  host: string;
  port: number;
}

export interface AgentSection {
  maxSteps: number;
  /** Path to icon_detect.onnx */
  yoloPath: string;
  /** YOLO detection confidence threshold. */
  boxThreshold: number;
  /** NMS IoU threshold. */
  iouThreshold: number;
  /** Enable verbose per-step logging (timing, tool calls, element counts). */
  debug: boolean;
  /** Timeout in ms for screenshot capture (0 = no timeout). */
  captureTimeoutMs: number;
  /**
   * Max screenshots carried as image blocks in the model context: only the most recent N are kept,
   * older ones are replaced with a text note. Bounds the dominant token cost on a vision model.
   */
  keepScreenshots: number;
  /**
   * Number of recent steps kept verbatim in the model context. Steps older than this are collapsed
   * into compact one-line summaries (element tables + screenshots dropped, actions retained).
   */
  contextWindow: number;
}
