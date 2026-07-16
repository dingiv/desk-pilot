/**
 * actions.ts — the composable pure action interfaces + their supporting data types.
 *
 * The brain's behavior is composed from small interfaces (design.md §5.1): `Observes` / `Acts` /
 * `Grounds` / `Completes`. An agent's role = which it composes. `Completes` / `Grounds` are also the
 * capability interfaces a {@link Model} mixes in (a chat model IS a `Completes`; a grounding model
 * IS a `Grounds`) — see `model.ts`. `Observes` / `Acts` are realized by platform-backed helpers here
 * (the SoM-mark path stays in `step.ts`; the pixel-direct path for grounding workers lives here).
 *
 * Status: introduces the §5.1 surface. The SoM `observe()`/`act()` impls remain in `step.ts` for now
 * (they will migrate to compose these interfaces in a later round); this module owns the new
 * pixel-grounding path (`captureObservation` + `performPlatformAction`).
 */
import type { CompleteRequest, LLMResponse } from '@vrover/llm';
import type { Platform } from '@vrover/platform';
import type { SoMElement } from '@vrover/som';

// ── data types (pure) ───────────────────────────────────────────────────────

/** A view of the world: a screenshot (always), an SoM table (SoM-mark path only). */
export interface Observation {
  readonly png?: Buffer;
  /** Element table for the SoM-mark path; absent when the agent grounds on pixels directly. */
  readonly somTable?: readonly SoMElement[];
}

/**
 * A pixel-level action — the single currency `Acts` consumes. SoM-mark agents resolve mark → bounds
 * → center into a `click`/`type` here; grounding models emit one directly. `done` ends the task.
 */
export type PlatformAction =
  | { kind: 'click'; x: number; y: number }
  | { kind: 'type'; text: string; x?: number; y?: number }
  | { kind: 'scroll'; x: number; y: number; dir: 'up' | 'down' }
  | { kind: 'keypress'; keys: readonly string[] }
  | { kind: 'done'; summary?: string };

/** Result of executing one action. `finished` marks a terminal `done`. */
export interface ActionResult {
  readonly ok: boolean;
  readonly message: string;
  readonly finished?: boolean;
}

// ── action interfaces (behavior, composable) ────────────────────────────────

/** Observe: produce the current Observation (GUI = screenshot (+ optional SoM); text = empty). */
export interface Observes {
  observe(): Promise<Observation>;
}

/** Act: land one pixel-level PlatformAction on a platform (the desktop tool surface). */
export interface Acts {
  act(action: PlatformAction): Promise<ActionResult>;
}

/** Chat completion capability (also a Model capability): messages(+tools) → text / tool_calls. */
export interface Completes {
  complete(req: CompleteRequest): Promise<LLMResponse>;
}

/** Pixel-grounding capability (also a Model capability): screenshot + hint → pixel action. */
export interface Grounds {
  ground(obs: Observation, hint: string): Promise<PlatformAction>;
}

// ── pixel-grounding path impls (GUI-TARS-class: capture only, act direct, no SoM/mark) ───────

/** Observe for the grounding path: capture a screenshot, no SoM annotation. */
export async function captureObservation(platform: Platform): Promise<Observation> {
  const shot = await platform.captureScreen();
  return { png: shot.png };
}

/** Execute a pixel-level PlatformAction directly on the platform (no mark → coord resolution). */
export async function performPlatformAction(
  action: PlatformAction,
  platform: Platform,
): Promise<ActionResult> {
  switch (action.kind) {
    case 'click':
      await platform.performClick(action.x, action.y);
      return { ok: true, message: `click(${action.x},${action.y})` };
    case 'type': {
      if (action.x !== undefined && action.y !== undefined) {
        await platform.performClick(action.x, action.y);
      }
      await platform.performType(action.text);
      return { ok: true, message: `type ${action.text.length} chars` };
    }
    case 'scroll':
      await platform.performScroll(action.x, action.y, action.dir);
      return { ok: true, message: `scroll ${action.dir}` };
    case 'keypress':
      await platform.performKeypress(action.keys.join('+'));
      return { ok: true, message: `keypress ${action.keys.join('+')}` };
    case 'done':
      return { ok: true, message: action.summary ?? 'done', finished: true };
  }
}
