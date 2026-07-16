import { createCanvas, type SKRSContext2D } from '@napi-rs/canvas';
import type { Bounds, Platform, Screenshot, UiElement } from '../types.js';
import { contains } from '../types.js';

/**
 * An in-memory {@link Platform} that renders the **Ubuntu (GNOME) Calculator** in
 * "Basic" mode with @napi-rs/canvas. It is the mock the agent can actually *use as a
 * calculator*: digits and + − × ÷ build an expression, `=` evaluates it for real.
 *
 * The reference UI lives in `./static/` (calc.main / calc.step_1 / calc.step_2) and the
 * scenario it must reproduce is the simple one in `./static/readme.md`:
 *   - start: empty display,
 *   - type `1+1` → display shows `1+1`,
 *   - click `=` → display shows `1+1 = 2`.
 *
 * Like the other mocks, the whole thing is in-memory so the agent loop, SoM annotation,
 * and LLM vision run on the **real** code path (real PNG in, real hit testing, a real
 * arithmetic core) with no OS, browser, or API key. Geometry lives in {@link buttonBounds}
 * / {@link displayBounds} so {@link getElements} and {@link render} never drift apart.
 *
 * The agent drives it two equivalent ways: click the Set-of-Mark buttons one at a time,
 * or `type` into the `display` element (the tool executor clicks it to focus, then types).
 */

// ── layout ───────────────────────────────────────────────────────────────────
// A 1280×800 canvas (same as the other mocks) with the calculator window centered on a
// plain desktop background — SoM marks live in the same coordinate space everywhere.
const W = 1280;
const H = 800;
const WIN = { x: 450, y: 130, w: 380, h: 540 };
const HEADER_H = 44;
const PAD = 10; // inset of the button grid inside the window
const GAP = 8; // gap between buttons
const COLS = 4;
const ROWS = 5;
const DISP = { y: WIN.y + HEADER_H + 6, h: 144 };
const GRID_Y = DISP.y + DISP.h + 8;
const GRID_BOTTOM = WIN.y + WIN.h - PAD;
const CELL_W = (WIN.w - 2 * PAD - (COLS - 1) * GAP) / COLS;
const CELL_H = (GRID_BOTTOM - GRID_Y - (ROWS - 1) * GAP) / ROWS;

type ButtonKind = 'digit' | 'op' | 'clear' | 'backspace' | 'equals';

interface ButtonDef {
  id: string;
  /** Glyph drawn on the button (and shown in the SoM table). */
  label: string;
  /** Token appended to the expression (pretty form, e.g. `÷` not `/`). */
  token: string;
  kind: ButtonKind;
  /** Accent button — the purple `=`. */
  accent?: boolean;
}

/**
 * The 4×5 Basic-mode grid, in row-major order. Tokens are stored in their pretty
 * (display) form; {@link evaluate} normalizes ÷/×/− to ASCII before parsing.
 */
const BUTTON_DEFS: readonly ButtonDef[] = [
  { id: 'clear', label: 'C', token: 'C', kind: 'clear' },
  { id: 'lparen', label: '(', token: '(', kind: 'op' },
  { id: 'rparen', label: ')', token: ')', kind: 'op' },
  { id: 'div', label: '÷', token: '÷', kind: 'op' },
  { id: 'd7', label: '7', token: '7', kind: 'digit' },
  { id: 'd8', label: '8', token: '8', kind: 'digit' },
  { id: 'd9', label: '9', token: '9', kind: 'digit' },
  { id: 'mul', label: '×', token: '×', kind: 'op' },
  { id: 'd4', label: '4', token: '4', kind: 'digit' },
  { id: 'd5', label: '5', token: '5', kind: 'digit' },
  { id: 'd6', label: '6', token: '6', kind: 'digit' },
  { id: 'sub', label: '−', token: '−', kind: 'op' },
  { id: 'd1', label: '1', token: '1', kind: 'digit' },
  { id: 'd2', label: '2', token: '2', kind: 'digit' },
  { id: 'd3', label: '3', token: '3', kind: 'digit' },
  { id: 'add', label: '+', token: '+', kind: 'op' },
  { id: 'd0', label: '0', token: '0', kind: 'digit' },
  { id: 'dot', label: '.', token: '.', kind: 'digit' },
  // ⌫/⌦ are missing from the canvas font (render as tofu); ← is the closest real glyph.
  { id: 'back', label: '←', token: '←', kind: 'backspace' },
  { id: 'eq', label: '=', token: '=', kind: 'equals', accent: true },
];

/** Map a typed/key-pressed character to the matching calculator input (pretty token). */
function charToInput(ch: string): { kind: ButtonKind; token: string } | null {
  switch (ch) {
    case '=':
    case '\n':
    case '\r':
      return { kind: 'equals', token: '=' };
    case '*':
    case '×':
      return { kind: 'op', token: '×' };
    case '/':
    case '÷':
      return { kind: 'op', token: '÷' };
    case '-':
    case '−':
    case '–':
      return { kind: 'op', token: '−' };
    case '+':
      return { kind: 'op', token: '+' };
    case '(':
      return { kind: 'op', token: '(' };
    case ')':
      return { kind: 'op', token: ')' };
    case '.':
    case ',':
      return { kind: 'digit', token: '.' };
  }
  if (/[0-9]/.test(ch)) return { kind: 'digit', token: ch };
  return null;
}

// ── expression evaluator ─────────────────────────────────────────────────────

/** Normalize the pretty expression (÷ × −) to ASCII operators the parser understands. */
function toAscii(expr: string): string {
  return expr.replace(/×/g, '*').replace(/÷/g, '/').replace(/[−–]/g, '-');
}

/**
 * Recursive-descent evaluation of `+ - * /`, parentheses, and unary `±`. Throws on any
 * malformed expression or divide-by-zero — the calculator turns that into an `Error`
 * display, exactly like a real one.
 *
 *   expr  = term (('+'|'-') term)*
 *   term  = factor (('*'|'/') factor)*
 *   factor= ('+'|'-') factor | '(' expr ')' | number
 */
function evaluateExpression(raw: string): number {
  const s = toAscii(raw).replace(/\s+/g, '');
  let i = 0;
  const eof = () => i >= s.length;
  const cur = () => s[i]!;

  function number(): number {
    let str = '';
    while (!eof() && /[0-9.]/.test(cur())) str += s[i++];
    if (!str) throw new Error('expected number');
    const n = Number(str);
    if (Number.isNaN(n)) throw new Error('bad number');
    return n;
  }
  function factor(): number {
    if (cur() === '+') return factor();
    if (cur() === '-') return -factor();
    if (cur() === '(') {
      i++;
      const v = expr();
      if (cur() !== ')') throw new Error('expected )');
      i++;
      return v;
    }
    return number();
  }
  function term(): number {
    let v = factor();
    while (!eof() && (cur() === '*' || cur() === '/')) {
      const op = cur();
      i++;
      const r = factor();
      if (op === '*') v *= r;
      else {
        if (r === 0) throw new Error('divide by zero');
        v /= r;
      }
    }
    return v;
  }
  function expr(): number {
    let v = term();
    while (!eof() && (cur() === '+' || cur() === '-')) {
      const op = cur();
      i++;
      const r = term();
      v = op === '+' ? v + r : v - r;
    }
    return v;
  }

  const out = expr();
  if (!eof()) throw new Error(`unexpected '${cur()}'`);
  return out;
}

/** Render a number for the display: integers as-is, else 12 sig-figs with trailing zeros trimmed. */
function formatNumber(n: number): string {
  if (!Number.isFinite(n)) throw new Error('non-finite');
  if (Number.isInteger(n)) return String(n);
  return String(Number(n.toPrecision(12)));
}

// ── platform ─────────────────────────────────────────────────────────────────

export class CalculatorPlatform implements Platform {
  readonly width = W;
  readonly height = H;

  /** The expression being built, in pretty form (e.g. `1+1`, `2×(3+4)`). */
  private expression = '';
  /** The last computed result, or `null` while still entering. */
  private result: string | null = null;
  /** Set when the last `=` hit an unparseable expression or divide-by-zero. */
  private error = false;

  /** What the display line currently reads — the single rendered line (matches the readme). */
  get display(): string {
    if (this.error) return 'Error';
    if (this.result !== null) return `${this.expression} = ${this.result}`;
    return this.expression || '0';
  }
  get currentExpression(): string {
    return this.expression;
  }
  get currentResult(): string | null {
    return this.result;
  }
  get isError(): boolean {
    return this.error;
  }

  // ── geometry: single source of truth for elements + rendering ──────────────

  private displayBounds(): Bounds {
    return { x: WIN.x + PAD, y: DISP.y, width: WIN.w - 2 * PAD, height: DISP.h };
  }

  private buttonBounds(index: number): Bounds {
    const col = index % COLS;
    const row = Math.floor(index / COLS);
    return {
      x: WIN.x + PAD + col * (CELL_W + GAP),
      y: GRID_Y + row * (CELL_H + GAP),
      width: CELL_W,
      height: CELL_H,
    };
  }

  /** Elements available for SoM annotation: the display (typeable) then every button. */
  get elements(): UiElement[] {
    const out: UiElement[] = [
      { id: 'display', role: 'input', label: 'Display', bounds: this.displayBounds() },
    ];
    BUTTON_DEFS.forEach((b, i) => {
      out.push({ id: b.id, role: 'button', label: b.label, bounds: this.buttonBounds(i) });
    });
    return out;
  }

  async getElements(): Promise<UiElement[]> {
    return this.elements;
  }

  async captureScreen(): Promise<Screenshot> {
    return { width: this.width, height: this.height, png: this.render() };
  }

  async performClick(x: number, y: number): Promise<void> {
    const hit = this.elements.find((el) => contains(el.bounds, x, y));
    if (!hit) return;
    if (hit.id === 'display') return; // focusing the display is a no-op here
    const def = BUTTON_DEFS.find((b) => b.id === hit.id);
    if (def) this.input(def.kind, def.token);
  }

  async performType(text: string): Promise<void> {
    for (const ch of text) {
      const m = charToInput(ch);
      if (m) this.input(m.kind, m.token);
    }
  }

  async performKeypress(keys: string): Promise<void> {
    if (/return|enter/i.test(keys)) return this.input('equals', '=');
    if (/escape/i.test(keys)) return this.input('clear', 'C');
    if (/backspace|delete/i.test(keys)) return this.input('backspace', '←');
    // Single-character keys (digits/operators/`=`) go straight in.
    if (keys.length === 1) {
      const m = charToInput(keys);
      if (m) this.input(m.kind, m.token);
    }
  }

  async performScroll(): Promise<void> {
    /* Nothing scrolls on a calculator. */
  }

  // ── the core state machine ─────────────────────────────────────────────────

  /** Apply one button/key input. The single entry point both clicks and typing funnel through. */
  private input(kind: ButtonKind, token: string): void {
    // Any non-`=` input recovers from an Error state by starting fresh.
    if (this.error && kind !== 'equals') {
      this.error = false;
      this.expression = '';
      this.result = null;
    }

    switch (kind) {
      case 'clear':
        this.expression = '';
        this.result = null;
        this.error = false;
        return;
      case 'backspace':
        // Backspacing off a computed result just drops the result; the expression stays.
        if (this.result !== null) {
          this.result = null;
          return;
        }
        this.expression = this.expression.slice(0, -1);
        return;
      case 'equals':
        this.evaluate();
        return;
      case 'digit':
      case 'op':
        this.append(token, kind);
        return;
    }
  }

  /** Append a digit/operator token, starting fresh off a computed result where a real calc would. */
  private append(token: string, kind: ButtonKind): void {
    if (this.result !== null) {
      if (kind === 'digit') {
        // A digit after `=` begins a brand-new expression.
        this.expression = token;
        this.result = null;
        return;
      }
      // An operator after `=` continues from the result.
      this.expression = this.result;
      this.result = null;
    }
    this.expression += token;
  }

  private evaluate(): void {
    const expr = this.expression.trim();
    if (!expr) return; // nothing to compute
    try {
      this.result = formatNumber(evaluateExpression(expr));
      this.error = false;
    } catch {
      this.error = true;
      this.result = null;
    }
  }

  // ── rendering ──────────────────────────────────────────────────────────────

  private render(): Buffer {
    const canvas = createCanvas(this.width, this.height);
    const ctx = canvas.getContext('2d');

    // Desktop background.
    ctx.fillStyle = '#e9ecef';
    ctx.fillRect(0, 0, this.width, this.height);

    this.drawWindow(ctx);
    this.drawHeader(ctx);
    this.drawDisplay(ctx);
    BUTTON_DEFS.forEach((b, i) => this.drawButton(ctx, b, this.buttonBounds(i)));

    return canvas.toBuffer('image/png');
  }

  private drawWindow(ctx: SKRSContext2D): void {
    ctx.fillStyle = '#ffffff';
    ctx.fillRect(WIN.x, WIN.y, WIN.w, WIN.h);
    ctx.strokeStyle = '#deddda';
    ctx.lineWidth = 1;
    ctx.strokeRect(WIN.x + 0.5, WIN.y + 0.5, WIN.w - 1, WIN.h - 1);
  }

  private drawHeader(ctx: SKRSContext2D): void {
    // Header strip.
    ctx.fillStyle = '#f6f5f4';
    ctx.fillRect(WIN.x, WIN.y, WIN.w, HEADER_H);
    ctx.strokeStyle = '#deddda';
    ctx.beginPath();
    ctx.moveTo(WIN.x, WIN.y + HEADER_H + 0.5);
    ctx.lineTo(WIN.x + WIN.w, WIN.y + HEADER_H + 0.5);
    ctx.stroke();

    // Mode title, centered like GNOME's calculator.
    ctx.fillStyle = '#5e5c64';
    ctx.font = '15px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'center';
    ctx.fillText('Basic ▾', WIN.x + WIN.w / 2, WIN.y + HEADER_H / 2 + 1);

    // Decorative window controls (minimize / maximize / close) — not interactive elements.
    const cy = WIN.y + HEADER_H / 2;
    const r = 6;
    const startX = WIN.x + WIN.w - 16;
    for (let k = 0; k < 3; k++) {
      ctx.beginPath();
      ctx.arc(startX - k * 18, cy, r, 0, Math.PI * 2);
      ctx.fillStyle = k === 0 ? '#e6616b' : '#d0cfc9';
      ctx.fill();
    }
  }

  private drawDisplay(ctx: SKRSContext2D): void {
    const { x, y, width, height } = this.displayBounds();

    // Faint separator under the display.
    ctx.strokeStyle = '#ececea';
    ctx.beginPath();
    ctx.moveTo(x, y + height + 0.5);
    ctx.lineTo(x + width, y + height + 0.5);
    ctx.stroke();

    const text = this.display;
    const isError = this.error;
    ctx.fillStyle = isError ? '#c01c28' : '#1d1d1d';
    ctx.textBaseline = 'alphabetic';
    ctx.textAlign = 'right';
    // Shrink the font for long expressions so they stay inside the display.
    const size = text.length > 18 ? 22 : text.length > 12 ? 30 : 38;
    ctx.font = `${size}px sans-serif`;
    ctx.fillText(text, x + width - 14, y + height - 16);
    ctx.textAlign = 'left';
  }

  private drawButton(ctx: SKRSContext2D, def: ButtonDef, b: Bounds): void {
    const { x, y, width, height } = b;

    let fill = '#ffffff';
    let stroke = '#deddda';
    let text = '#1d1d1d';
    if (def.accent) {
      fill = '#813d9c'; // GNOME/Ubuntu purple — the distinctive `=`
      stroke = '#813d9c';
      text = '#ffffff';
    } else if (def.kind === 'op') {
      fill = '#f6f5f4';
      text = '#3d3846';
    } else if (def.kind === 'clear' || def.kind === 'backspace') {
      fill = '#f0eee9';
      text = '#c01c28';
    }

    roundRect(ctx, x, y, width, height, 8);
    ctx.fillStyle = fill;
    ctx.fill();
    ctx.strokeStyle = stroke;
    ctx.lineWidth = 1;
    ctx.stroke();

    ctx.fillStyle = text;
    ctx.font = 'bold 24px sans-serif';
    ctx.textBaseline = 'middle';
    ctx.textAlign = 'center';
    ctx.fillText(def.label, x + width / 2, y + height / 2 + 1);
    ctx.textAlign = 'left';
  }
}

/** Stroke-and-fill-friendly rounded rectangle (napi-rs/canvas roundRect support varies). */
function roundRect(ctx: SKRSContext2D, x: number, y: number, w: number, h: number, r: number): void {
  const rr = Math.min(r, w / 2, h / 2);
  ctx.beginPath();
  ctx.moveTo(x + rr, y);
  ctx.arcTo(x + w, y, x + w, y + h, rr);
  ctx.arcTo(x + w, y + h, x, y + h, rr);
  ctx.arcTo(x, y + h, x, y, rr);
  ctx.arcTo(x, y, x + w, y, rr);
  ctx.closePath();
}
