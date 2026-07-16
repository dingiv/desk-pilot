/**
 * @vrover/agent prompt management — named, templated prompts in a registry. Text lives in
 * `./constants.js`; the loop renders `system` (identity+rules), `step` (per-turn screen context),
 * and `nudge` (when the model stalls) via {@link prompts}.render.
 */
export { PromptRegistry, prompts, render } from './registry.js';
export type { PromptName, PromptVars } from './types.js';
export { SYSTEM_PROMPT, STEP_TEMPLATE, NUDGE, COMPACT_TEMPLATE } from './constants.js';
