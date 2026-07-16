import { COMPACT_TEMPLATE, NUDGE, STEP_TEMPLATE, SYSTEM_PROMPT } from './constants.js';
import type { PromptName, PromptVars } from './types.js';

/**
 * Named prompt registry. Each name maps to a template string (from {@link "./constants.js"}) with
 * `{{var}}` placeholders; {@link PromptRegistry.render} substitutes them. Adding a prompt =
 * adding a constant in `./constants.js`, a name in {@link PromptName}, and an entry below.
 */
export class PromptRegistry {
  private readonly templates: Record<PromptName, string>;

  constructor(templates: Record<PromptName, string>) {
    this.templates = templates;
  }

  /** The raw (un-rendered) template for a prompt. */
  get(name: PromptName): string {
    return this.templates[name]!;
  }

  /** Render a prompt, substituting `{{key}}` placeholders from `vars` (unknowns left as-is). */
  render(name: PromptName, vars?: PromptVars): string {
    return substitute(this.templates[name]!, vars);
  }
}

function substitute(template: string, vars?: PromptVars): string {
  if (!vars) return template;
  return template.replace(/\{\{(\w+)\}\}/g, (match, key: string) => {
    const v = vars[key];
    return v === undefined ? match : String(v);
  });
}

/** The default registry, seeded from the prompt constants. */
export const prompts = new PromptRegistry({
  system: SYSTEM_PROMPT,
  step: STEP_TEMPLATE,
  nudge: NUDGE,
  compact: COMPACT_TEMPLATE,
});

/** Convenience: render via the default {@link prompts} registry. */
export function render(name: PromptName, vars?: PromptVars): string {
  return prompts.render(name, vars);
}
