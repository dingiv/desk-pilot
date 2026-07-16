import type { Platform, UiElement } from '@vrover/platform';
import { centerOf } from '@vrover/platform';
import type { SoMElement } from '@vrover/som';

/** What running one tool call produced. */
export interface DispatchResult {
  /** Text fed back to the model as the tool_result. */
  message: string;
  /** True when the call signals task completion (only `done`). */
  finished: boolean;
  summary?: string;
}

function describe(el: UiElement): string {
  return el.label.trim() ? `[${el.role}] ${el.label}` : `[${el.role}]`;
}

function lookupMark(table: SoMElement[], mark: unknown): SoMElement {
  const n = Number(mark);
  const found = table.find((e) => e.mark === n);
  if (!found) {
    throw new Error(
      `No element with mark ${mark}. Available marks: ${table.map((e) => e.mark).join(', ')}`,
    );
  }
  return found;
}

/**
 * Resolve a tool call against the current SoM table and run it on the platform. mark → element →
 * center coordinate → Platform primitive. Throws on bad input; the agent loop turns that into an
 * `is_error` tool_result so the model can correct itself.
 */
export async function dispatch(
  name: string,
  input: Record<string, unknown>,
  table: SoMElement[],
  platform: Platform,
): Promise<DispatchResult> {
  switch (name) {
    case 'click': {
      const { element } = lookupMark(table, input.mark);
      const c = centerOf(element.bounds);
      await platform.performClick(c.x, c.y);
      return { message: `Clicked ${describe(element)}.`, finished: false };
    }
    case 'type': {
      const { element } = lookupMark(table, input.mark);
      const text = String(input.text ?? '');
      const c = centerOf(element.bounds);
      await platform.performClick(c.x, c.y); // focus the field first
      await platform.performType(text);
      return { message: `Typed "${text}" into ${describe(element)}.`, finished: false };
    }
    case 'scroll': {
      const { element } = lookupMark(table, input.mark);
      const direction = input.direction === 'down' ? 'down' : 'up';
      const c = centerOf(element.bounds);
      await platform.performScroll(c.x, c.y, direction);
      return { message: `Scrolled ${direction} near ${describe(element)}.`, finished: false };
    }
    case 'keypress': {
      const keys = String(input.keys ?? '');
      await platform.performKeypress(keys);
      return { message: `Pressed ${keys}.`, finished: false };
    }
    case 'done': {
      const summary = String(input.summary ?? 'Task complete.');
      return { message: `Done: ${summary}`, finished: true, summary };
    }
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
}
