import Anthropic from '@anthropic-ai/sdk';
import { loadConfig } from './config.js';
import type { ContentBlock, CompleteRequest, LLMResponse, Message } from './types.js';

/**
 * The one place VRover talks to Anthropic. All SDK calls live here, so when a second provider
 * arrives it's a sibling module exporting a function with the same {@link CompleteFn} signature —
 * the agent loop and the rest of the codebase never import the SDK directly.
 *
 * v1 deliberately disables extended thinking (omits the `thinking` param) to keep message
 * history management simple; flip on adaptive thinking here when grounding gets harder.
 */

let client: Anthropic | null = null;

function getClient(): Anthropic {
  if (!client) {
    client = new Anthropic({ apiKey: loadConfig().anthropicApiKey });
  }
  return client;
}

/** Convert our content blocks to the SDK's request shape. */
function toSdkContent(blocks: ContentBlock[]): Anthropic.ContentBlockParam[] {
  return blocks.map((b): Anthropic.ContentBlockParam => {
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
        return { type: 'tool_result', tool_use_id: b.tool_use_id, content: b.content, is_error: b.is_error };
    }
  });
}

function toSdkMessages(messages: Message[]): Anthropic.MessageParam[] {
  return messages.map((m) => ({ role: m.role, content: toSdkContent(m.content) }));
}

/** Run one model turn. Throws on API errors — the agent loop surfaces them as `status: 'error'`. */
export async function complete(req: CompleteRequest): Promise<LLMResponse> {
  const cfg = loadConfig();
  const resp = await getClient().messages.create({
    model: cfg.model,
    max_tokens: cfg.maxTokens,
    system: req.system,
    // z.toJSONSchema emits valid JSON Schema (with `type`) at runtime; the provider-agnostic
    // ToolDef types it as a loose record, so cast to the SDK shape at this boundary.
    tools: req.tools.map((t) => ({
      name: t.name,
      description: t.description,
      input_schema: t.input_schema,
    })) as unknown as Anthropic.Tool[],
    messages: toSdkMessages(req.messages),
    output_config: { effort: cfg.effort },
  });

  let text: string | null = null;
  const toolUses: LLMResponse['toolUses'] = [];
  const raw: ContentBlock[] = [];

  for (const block of resp.content) {
    if (block.type === 'text') {
      text = (text ?? '') + block.text;
      raw.push({ type: 'text', text: block.text });
    } else if (block.type === 'tool_use') {
      const input = block.input as Record<string, unknown>;
      toolUses.push({ id: block.id, name: block.name, input });
      raw.push({ type: 'tool_use', id: block.id, name: block.name, input });
    }
    // thinking / redacted_thinking blocks: not produced with thinking disabled in v1.
  }

  return { text, toolUses, raw, stopReason: resp.stop_reason };
}
