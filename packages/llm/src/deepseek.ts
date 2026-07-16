import { loadConfig } from './config.js';
import type { ContentBlock, CompleteRequest, LLMResponse, Message } from './types.js';

/**
 * The one place VRover talks to DeepSeek. Sibling to {@link "./anthropic.js"}: same
 * {@link CompleteFn} signature and the same Anthropic wire format, but DeepSeek is reached via its
 * Anthropic-compatible endpoint (`<baseUrl>/v1/messages`) using `fetch` directly — no SDK dep —
 * with its own base URL / model / key sourced from {@link loadConfig} (`llm.deepseek`).
 *
 * Reads config lazily inside the call (like `anthropic.ts`'s `getClient`), so importing this module
 * and even building a Model around it stays key-free at boot; the key is only touched on the first
 * real turn. Throws on a missing key at call time rather than killing the process — a library
 * `CompleteFn` should surface errors, not exit.
 */

/** DeepSeek shares Anthropic's content-block wire shape; this mirrors it without the SDK. */
function toWireContent(blocks: ContentBlock[]): unknown[] {
  return blocks.map((b) => {
    switch (b.type) {
      case 'text':
        return { type: 'text', text: b.text };
      case 'image':
        return {
          type: 'image',
          source: { type: 'base64', media_type: b.mediaType, data: b.data.toString('base64') },
        };
      case 'tool_use':
        return { type: 'tool_use', id: b.id, name: b.name, input: b.input };
      case 'tool_result':
        return {
          type: 'tool_result',
          tool_use_id: b.tool_use_id,
          content: b.content,
          is_error: b.is_error,
        };
    }
  });
}

function toWireMessages(messages: Message[]): unknown[] {
  return messages.map((m) => ({ role: m.role, content: toWireContent(m.content) }));
}

/** Run one DeepSeek model turn. Throws on API/auth errors — the agent loop surfaces them. */
export async function deepseekComplete(req: CompleteRequest): Promise<LLMResponse> {
  const { apiKey, baseUrl, model } = loadConfig().deepseek;
  if (!apiKey) {
    throw new Error(
      'DeepSeek API key is not set. Configure llm.deepseek.apiKey in vrover.conf or set DEEPSEEK_API_KEY.',
    );
  }

  const body = JSON.stringify({
    model,
    max_tokens: 4096,
    system: req.system,
    tools: req.tools.map((t) => ({
      name: t.name,
      description: t.description,
      input_schema: t.input_schema,
    })),
    messages: toWireMessages(req.messages),
  });

  const resp = await fetch(`${baseUrl}/v1/messages`, {
    method: 'POST',
    headers: {
      'x-api-key': apiKey,
      'anthropic-version': '2023-06-01',
      'content-type': 'application/json',
    },
    body,
  });

  if (!resp.ok) {
    const errText = await resp.text().catch(() => resp.statusText);
    throw new Error(`deepseek API ${resp.status}: ${errText.slice(0, 300)}`);
  }

  const data = (await resp.json()) as { content?: Array<{ type: string; text?: string; id?: string; name?: string; input?: Record<string, unknown> }>; stop_reason?: string };
  let text: string | null = null;
  const toolUses: LLMResponse['toolUses'] = [];
  const raw: ContentBlock[] = [];

  for (const block of data.content ?? []) {
    if (block.type === 'text') {
      text = (text ?? '') + (block.text ?? '');
      raw.push({ type: 'text', text: block.text ?? '' });
    } else if (block.type === 'tool_use') {
      const input = (block.input ?? {}) as Record<string, unknown>;
      toolUses.push({ id: block.id ?? '', name: block.name ?? '', input });
      raw.push({ type: 'tool_use', id: block.id ?? '', name: block.name ?? '', input });
    }
  }

  return { text, toolUses, raw, stopReason: data.stop_reason ?? null };
}
