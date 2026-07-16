import type { ContentBlock, Message } from '@vrover/llm';
import { prompts } from './prompts/index.js';

/**
 * Context management for the agent loop. The loop keeps the full verbatim `history` as its source
 * of truth; {@link pruneForModel} derives the bounded message list actually sent to the model.
 *
 * Two pure transforms compose (no I/O; `history` is never mutated):
 *  1. **Compact** — turns beyond `contextWindow` collapse into one `user` text message (one
 *     action line per step; element tables + screenshots dropped). The task preamble and the
 *     recent window stay verbatim.
 *  2. **Cap images** — across the verbatim portion, only the `keepScreenshots` most recent image
 *     blocks survive; older ones become a `[screenshot from step N omitted]` note.
 *
 * A "turn" is one observe→think→act cycle. An observe message is a `user` message carrying an
 * image block (both the Rust `nativeParser` path and the TS `annotate()` path always emit
 * `som.annotated.png`), so an image block reliably marks a turn boundary.
 */

/** Options for {@link pruneForModel}. */
export interface PruneOptions {
  /** Recent turns kept verbatim. Older turns are compacted to one action line each. */
  contextWindow: number;
  /** Most-recent image blocks retained; older ones become a text omission note. */
  keepScreenshots: number;
}

/** Derive the bounded message list sent to the model from the full verbatim `history`. */
export function pruneForModel(history: Message[], opts: PruneOptions): Message[] {
  const { preamble, turns } = groupTurns(history);
  const keepTurns = Math.max(0, opts.contextWindow);
  const split = Math.max(0, turns.length - keepTurns);
  const oldTurns = turns.slice(0, split);
  const recentTurns = turns.slice(split);

  const out: Message[] = [...preamble];
  if (oldTurns.length > 0) out.push(compactSummary(oldTurns));
  for (const turn of recentTurns) out.push(...turn);
  return capImages(out, Math.max(0, opts.keepScreenshots));
}

// ── grouping ────────────────────────────────────────────────────────────────

interface TurnGroup {
  /** Leading messages before the first observe (normally the task message). */
  preamble: Message[];
  /** Each group = [observeMsg, assistantMsg, resultsOrNudgeMsg, …] up to the next observe. */
  turns: Message[][];
}

/** Split `history` into the task preamble + per-turn message groups (observe message = boundary). */
function groupTurns(history: Message[]): TurnGroup {
  const preamble: Message[] = [];
  const turns: Message[][] = [];
  let i = 0;
  while (i < history.length && !hasImage(history[i]!)) {
    preamble.push(history[i]!);
    i++;
  }
  while (i < history.length) {
    const start = i; // the observe message (carries an image)
    i++;
    while (i < history.length && !hasImage(history[i]!)) i++;
    turns.push(history.slice(start, i));
  }
  return { preamble, turns };
}

/**
 * History length after each completed turn (preamble length + each turn's messages, in order).
 * The boundaries `goto` truncates to — derived from history, so a restored task needs no extra
 * persisted state.
 */
export function turnBoundaries(history: Message[]): number[] {
  const { preamble, turns } = groupTurns(history);
  const out: number[] = [];
  let acc = preamble.length;
  for (const turn of turns) {
    acc += turn.length;
    out.push(acc);
  }
  return out;
}

function hasImage(m: Message): boolean {
  return m.content.some((b) => b.type === 'image');
}

// ── compaction ──────────────────────────────────────────────────────────────

/** Collapse a run of older turns into one compact `user` summary message. */
function compactSummary(turns: Message[][]): Message {
  const steps = turns.map((turn, idx) => compactTurnLine(turn, idx + 1)).join('\n');
  return {
    role: 'user',
    content: [{ type: 'text', text: prompts.render('compact', { steps }) }],
  };
}

/**
 * One compact action line for a turn, e.g.
 *   `step 3: click(mark=1) → ok; type(mark=2, text='x') → done`
 * or, when the model only talked, `step 3: said: <first line>`.
 */
function compactTurnLine(turn: Message[], fallbackStep: number): string {
  const stepNo = turnStepNumber(turn[0], fallbackStep);
  const calls: string[] = [];
  const results: string[] = [];
  let said = '';
  for (const msg of turn) {
    for (const b of msg.content) {
      if (b.type === 'tool_use') {
        calls.push(`${b.name}(${fmtInputShort(b.input)})`);
      } else if (b.type === 'tool_result') {
        results.push(shorten(b.content));
      } else if (b.type === 'text' && msg.role === 'assistant' && !said) {
        said = firstLine(b.text);
      }
    }
  }

  let body: string;
  if (calls.length > 0) {
    body = calls
      .map((c, i) => {
        const r = results[i];
        return r !== undefined ? `${c} → ${r}` : c;
      })
      .join('; ');
  } else {
    body = said ? `said: ${shorten(said)}` : '(no action)';
  }
  return `step ${stepNo}: ${body}`;
}

/** Prefer the step number baked into the observe text; fall back to the group index. */
function turnStepNumber(msg: Message | undefined, fallback: number): number {
  if (msg) {
    const n = stepNumberOf(msg);
    if (n !== undefined) return n;
  }
  return fallback;
}

// ── image capping ───────────────────────────────────────────────────────────

/** Keep only the `keep` most-recent image blocks; replace the rest with an omission note. */
function capImages(messages: Message[], keep: number): Message[] {
  const locs: Array<[number, number]> = [];
  for (let mi = 0; mi < messages.length; mi++) {
    const blocks = messages[mi]!.content;
    for (let bi = 0; bi < blocks.length; bi++) {
      if (blocks[bi]!.type === 'image') locs.push([mi, bi]);
    }
  }
  const dropCount = Math.max(0, locs.length - Math.max(0, keep));
  if (dropCount === 0) return messages;
  const dropped = new Set(locs.slice(0, dropCount).map(([m, b]) => `${m}:${b}`));

  return messages.map((m, mi) => {
    const stepNo = stepNumberOf(m);
    const note = stepNo ? `[screenshot from step ${stepNo} omitted]` : '[screenshot omitted]';
    let changed = false;
    const content: ContentBlock[] = m.content.map((blk, bi) => {
      if (blk.type === 'image' && dropped.has(`${mi}:${bi}`)) {
        changed = true;
        return { type: 'text', text: note };
      }
      return blk;
    });
    return changed ? { role: m.role, content } : m;
  });
}

/** Extract `N` from an observe message's "Current screen (step N)…" text, if present. */
function stepNumberOf(m: Message): number | undefined {
  for (const b of m.content) {
    if (b.type === 'text') {
      const match = /step\s+(\d+)/i.exec(b.text);
      if (match) return Number(match[1]);
    }
  }
  return undefined;
}

// ── formatting ──────────────────────────────────────────────────────────────

function fmtInputShort(input: Record<string, unknown>): string {
  const entries = Object.entries(input);
  if (entries.length === 0) return '';
  return entries.map(([k, v]) => `${k}=${fmtVal(v)}`).join(', ');
}

function fmtVal(v: unknown): string {
  if (typeof v === 'string') return `'${v}'`;
  if (v === null) return 'null';
  return String(v);
}

function firstLine(s: string): string {
  return s.split('\n', 1)[0]!.trim();
}

function shorten(s: string): string {
  const t = s.trim().replace(/\s+/g, ' ');
  return t.length > 48 ? `${t.slice(0, 45)}…` : t;
}
