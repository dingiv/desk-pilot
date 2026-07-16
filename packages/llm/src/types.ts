/**
 * Provider-agnostic LLM protocol types.
 *
 * Today only {@link "./anthropic.js"} implements this shape (single-provider, per the plan).
 * These types are a thin mirror of the wire format so the agent loop doesn't import the SDK
 * directly — when a second provider arrives, it just needs to match this signature.
 */

export type Role = 'user' | 'assistant';

/** Content blocks exchanged with the model. Translated to/from provider formats by llm/*.ts. */
export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; mediaType: 'image/png'; data: Buffer }
  | { type: 'tool_use'; id: string; name: string; input: Record<string, unknown> }
  | { type: 'tool_result'; tool_use_id: string; content: string; is_error?: boolean };

export interface Message {
  role: Role;
  content: ContentBlock[];
}

/** A tool definition handed to the model. `input_schema` is a JSON Schema object. */
export interface ToolDef {
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
}

/** What the LLM returned for one turn. */
export interface LLMResponse {
  /** Plain text the model emitted, if any (besides tool calls). */
  text: string | null;
  toolUses: { id: string; name: string; input: Record<string, unknown> }[];
  /** Raw assistant content blocks, to push back into history verbatim. */
  raw: ContentBlock[];
  stopReason: string | null;
}

/** Input to a provider's `complete` call. */
export interface CompleteRequest {
  system: string;
  messages: Message[];
  tools: ToolDef[];
}

/**
 * The single LLM exit point. The agent loop takes this as a dependency so it can run against a
 * fake in tests without touching the API. Today only `./anthropic.ts` provides a real one;
 * adding a provider = adding a function with this signature.
 */
export type CompleteFn = (req: CompleteRequest) => Promise<LLMResponse>;

