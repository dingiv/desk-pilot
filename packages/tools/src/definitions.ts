import { z } from 'zod';
import type { ToolDef } from '@vrover/llm';

/**
 * The agent's tool surface. The model always acts by calling one of these, referencing a UI
 * element by its **mark** (the red number SoM draws over it) — never raw coordinates. The tool
 * executor (see `./executor.ts`) resolves mark → element → center → Platform primitive.
 *
 * Schemas are written in Zod and converted to JSON Schema for the model; that's the only place
 * the wire format is described, so the two can't drift.
 */

const clickSchema = z.object({
  mark: z.number().int().describe('The red number drawn over the element to click.'),
});

const typeSchema = z.object({
  mark: z.number().int().describe('The red number drawn over the input element.'),
  text: z.string().describe('Text to type into the element (it is focused first).'),
});

const scrollSchema = z.object({
  mark: z.number().int().describe('The red number drawn over an element to scroll near.'),
  direction: z.enum(['up', 'down']).describe('Direction to scroll.'),
});

const keypressSchema = z.object({
  keys: z.string().describe('Key or combo, e.g. "Return", "ctrl+s".'),
});

const doneSchema = z.object({
  summary: z.string().describe('Short summary of what was accomplished.'),
});

function jsonSchema(schema: z.ZodType): Record<string, unknown> {
  // Drop the `$schema` marker so the shape matches Anthropic's documented input_schema.
  const json = z.toJSONSchema(schema) as Record<string, unknown>;
  delete json.$schema;
  return json;
}

function tool(name: string, description: string, schema: z.ZodType): ToolDef {
  return { name, description, input_schema: jsonSchema(schema) };
}

export const TOOL_DEFS: ToolDef[] = [
  tool('click', 'Click the element identified by its mark number.', clickSchema),
  tool('type', 'Type text into the input element identified by its mark. Focuses it first.', typeSchema),
  tool('scroll', 'Scroll up or down near the element identified by its mark.', scrollSchema),
  tool('keypress', 'Press a key or key combination, e.g. "Return" or "ctrl+s".', keypressSchema),
  tool('done', 'Call when the task is complete. Provide a short summary.', doneSchema),
];
