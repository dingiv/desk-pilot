import type {
  CompleteFn,
  CompleteRequest,
  ContentBlock,
  LLMResponse,
  Message,
} from '@vrover/llm';

/**
 * Options for an OpenAI-compatible Chat Completions endpoint. One shape covers GLM-native,
 * OpenAI, vLLM, llama.cpp, LM Studio and Ollama — they all speak the same wire format; only
 * the base URL, auth and model name differ (see `./glm.js` and `./vllm.js` for presets).
 */
export interface OpenAICompatibleOptions {
  /**
   * Base URL ending in the version segment, e.g. `https://open.bigmodel.cn/api/paas/v4`,
   * `http://localhost:8000/v1`. The adapter appends `/chat/completions`.
   */
  baseUrl: string;
  /** API key, sent as `Authorization: Bearer <key>`. Omitted when empty (no-auth servers). */
  apiKey?: string;
  /** Model name as the server expects it (its `--served-model-name` for local servers). */
  model: string;
  /** Max output tokens per turn (default 4096). */
  maxTokens?: number;
}

// ---- OpenAI Chat Completions wire types (the shape we translate to/from) ----------

interface OAIContentPart {
  type: 'text' | 'image_url';
  text?: string;
  image_url?: { url: string };
}

interface OAIToolCall {
  id: string;
  type: 'function';
  function: { name: string; arguments: string };
}

interface OAIMessage {
  role: 'system' | 'user' | 'assistant' | 'tool';
  content: string | OAIContentPart[] | null;
  tool_calls?: OAIToolCall[];
  tool_call_id?: string;
}

interface OAIChoice {
  message: { content: string | null; tool_calls?: OAIToolCall[] };
  finish_reason: string | null;
}

// ---- request translation: CompleteRequest → OpenAI body --------------------------

function toAIMessages(req: CompleteRequest): OAIMessage[] {
  const out: OAIMessage[] = [];
  if (req.system) out.push({ role: 'system', content: req.system });
  for (const m of req.messages) out.push(...toAIMessage(m));
  return out;
}

/** A user/assistant message → one or more OpenAI messages (a tool-result user message splits). */
function toAIMessage(m: Message): OAIMessage[] {
  if (m.role === 'assistant') return [assistantToAIMessage(m.content)];

  // User message: the loop sends either tool_results (→ split into tool-role messages) or
  // ordinary text/image content — never mixed. Branch on whether any tool_result is present.
  if (m.content.some((b) => b.type === 'tool_result')) {
    return m.content
      .filter((b): b is Extract<ContentBlock, { type: 'tool_result' }> => b.type === 'tool_result')
      .map((b) => ({ role: 'tool', content: b.content, tool_call_id: b.tool_use_id }));
  }
  return [{ role: 'user', content: userContent(m.content) }];
}

function assistantToAIMessage(content: ContentBlock[]): OAIMessage {
  const text = content
    .filter((b): b is Extract<ContentBlock, { type: 'text' }> => b.type === 'text')
    .map((b) => b.text)
    .join('');
  const toolCalls: OAIToolCall[] = content
    .filter((b): b is Extract<ContentBlock, { type: 'tool_use' }> => b.type === 'tool_use')
    .map((b) => ({
      id: b.id,
      type: 'function',
      function: { name: b.name, arguments: JSON.stringify(b.input) },
    }));
  return {
    role: 'assistant',
    // OpenAI (and GLM/vLLM) prefer null over "" when only tool_calls are present.
    content: text.length ? text : toolCalls.length ? null : '',
    ...(toolCalls.length ? { tool_calls: toolCalls } : {}),
  };
}

/** User text/image blocks → a plain string (single text) or an OpenAI content-parts array. */
function userContent(content: ContentBlock[]): string | OAIContentPart[] {
  const parts: OAIContentPart[] = [];
  for (const b of content) {
    if (b.type === 'text') {
      parts.push({ type: 'text', text: b.text });
    } else if (b.type === 'image') {
      parts.push({
        type: 'image_url',
        image_url: { url: `data:${b.mediaType};base64,${b.data.toString('base64')}` },
      });
    }
    // tool_use/tool_result never appear in a user content message; ignore defensively.
  }
  const only = parts[0];
  if (parts.length === 1 && only?.type === 'text') return only.text ?? '';
  return parts;
}

function toAITools(tools: CompleteRequest['tools']) {
  return tools.map((t) => ({
    type: 'function' as const,
    function: { name: t.name, description: t.description, parameters: t.input_schema },
  }));
}

// ---- response translation: OpenAI choice → LLMResponse ---------------------------

/** Parse OpenAI's JSON-string `arguments`; degrade to `{ _raw }` if malformed. */
function parseArguments(args: string): Record<string, unknown> {
  try {
    const parsed = JSON.parse(args);
    return parsed && typeof parsed === 'object' ? (parsed as Record<string, unknown>) : { _raw: args };
  } catch {
    return { _raw: args };
  }
}

/**
 * Build a {@link CompleteFn} against any OpenAI-compatible endpoint. Uses the global `fetch`
 * (no SDK, no extra dependency) so it runs against any server, local or remote. Throws on
 * non-2xx — the agent loop surfaces that as `status: 'error'`.
 */
export function createOpenAICompatible(opts: OpenAICompatibleOptions): CompleteFn {
  const baseUrl = opts.baseUrl.replace(/\/+$/, '');
  const maxTokens = opts.maxTokens ?? 4096;

  return async (req: CompleteRequest): Promise<LLMResponse> => {
    const messages = toAIMessages(req);
    const body: Record<string, unknown> = { model: opts.model, messages, max_tokens: maxTokens };
    const tools = toAITools(req.tools);
    if (tools.length) body.tools = tools; // some servers reject `tools: []`

    const headers: Record<string, string> = { 'content-type': 'application/json' };
    if (opts.apiKey) headers.authorization = `Bearer ${opts.apiKey}`;

    const resp = await fetch(`${baseUrl}/chat/completions`, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });
    if (!resp.ok) {
      const detail = await resp.text().catch(() => '');
      throw new Error(`HTTP ${resp.status} ${resp.statusText}: ${detail}`);
    }

    const data = (await resp.json()) as { choices?: OAIChoice[] };
    const choice = data.choices?.[0];
    const msg = choice?.message;

    const raw: ContentBlock[] = [];
    let text: string | null = null;
    if (typeof msg?.content === 'string' && msg.content) {
      text = msg.content;
      raw.push({ type: 'text', text: msg.content });
    }

    const toolUses: LLMResponse['toolUses'] = (msg?.tool_calls ?? []).map((tc) => {
      const input = parseArguments(tc.function.arguments);
      // CRITICAL: reuse the server's tool_call id verbatim in `raw`, so the next turn's
      // tool_result.tool_use_id ↔ OpenAI tool_call_id linkage stays intact across turns.
      raw.push({ type: 'tool_use', id: tc.id, name: tc.function.name, input });
      return { id: tc.id, name: tc.function.name, input };
    });

    return { text, toolUses, raw, stopReason: choice?.finish_reason ?? null };
  };
}
