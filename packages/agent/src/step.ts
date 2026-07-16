import type { ContentBlock } from '@vrover/llm';
import type { NativeParser } from '@vrover/native';
import { convertToSoMResult } from '@vrover/native';
import type { Platform } from '@vrover/platform';
import type { SoMElement, SoMResult } from '@vrover/som';
import { annotate } from '@vrover/som';
import type { DispatchFn, StepAction } from './types.js';

/**
 * The pure-ish building blocks of one agent step, shared by the stateful {@link Agent} (`agent.ts`)
 * and the legacy `runAgent` loop. `observe` + `act` are the composition seam — the agent orchestrates
 * state and delegates the actual screen-grab / tool execution here.
 */

/**
 * observe: capture → detect + annotate → SoM.
 *
 * Two paths:
 * 1. **Rust** (when `nativeParser` is set): calls `parser.parse(png)` — YOLO
 *    detection + SoM annotation in a single Rust pass. No `getElements()` or
 *    TS `annotate()` needed.
 * 2. **TS** (fallback): `platform.getElements()` + `annotate()` using
 *    `@napi-rs/canvas`. Used when no native parser is wired (tests, mocks,
 *    remote platforms).
 */
export async function observe(
  platform: Platform,
  nativeParser: NativeParser | undefined,
  captureTimeoutMs: number | undefined,
): Promise<SoMResult> {
  const screenshot = await withTimeout(
    platform.captureScreen(),
    captureTimeoutMs ?? 0,
    'captureScreen',
  );

  if (nativeParser) {
    const result = nativeParser.parse(screenshot.png);
    return convertToSoMResult(screenshot, result);
  }

  const elements = await platform.getElements();
  return annotate(screenshot, elements);
}

/** act: run each tool call via the dispatcher, collecting actions + tool_result blocks. */
export async function act(
  toolUses: { id: string; name: string; input: Record<string, unknown> }[],
  table: SoMElement[],
  platform: Platform,
  runTool: DispatchFn,
  log: (message: string) => void,
): Promise<{
  actions: StepAction[];
  toolResults: ContentBlock[];
  finished: boolean;
  summary?: string;
}> {
  const actions: StepAction[] = [];
  const toolResults: ContentBlock[] = [];
  let finished = false;
  let summary: string | undefined;

  for (const tu of toolUses) {
    try {
      const r = await runTool(tu.name, tu.input, table, platform);
      actions.push({ name: tu.name, input: tu.input, result: r.message });
      toolResults.push({ type: 'tool_result', tool_use_id: tu.id, content: r.message });
      log(`  → ${tu.name}(${fmtInput(tu.input)}) → ${r.message}`);
      if (r.finished) {
        finished = true;
        summary = r.summary;
      }
    } catch (err) {
      const m = errMsg(err);
      actions.push({ name: tu.name, input: tu.input, result: `error: ${m}` });
      toolResults.push({
        type: 'tool_result',
        tool_use_id: tu.id,
        content: `Error: ${m}`,
        is_error: true,
      });
      log(`  → ${tu.name}(${fmtInput(tu.input)}) → ERROR: ${m}`);
    }
  }

  return { actions, toolResults, finished, summary };
}

/** Render an error into a string (Error → message, anything else → String). */
export function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function fmtInput(input: Record<string, unknown>): string {
  return JSON.stringify(input);
}

/** Race a promise against a timeout. Returns the promise result or throws. */
async function withTimeout<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  if (ms <= 0) return promise;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}
