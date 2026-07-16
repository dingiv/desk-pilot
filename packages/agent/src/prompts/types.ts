/** Named prompts the agent loop renders (see {@link "./registry.js"}). */
export type PromptName = 'system' | 'step' | 'nudge' | 'compact';

/** Variable values substituted into a template (`{{key}}` → value). */
export type PromptVars = Record<string, string | number>;
