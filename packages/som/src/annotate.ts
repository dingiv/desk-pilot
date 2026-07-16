import { createCanvas, Image } from '@napi-rs/canvas';
import type { Screenshot, UiElement } from '@vrover/platform';
import type { SoMElement, SoMResult } from './types.js';

export interface AnnotateOptions {
  /** Max elements to annotate (extras are dropped). Default 50. */
  maxElements?: number;
}

/** Describe an element for the table, e.g. "[button] Login". */
export function describeElement(el: UiElement): string {
  const label = el.label.trim();
  return label ? `[${el.role}] ${label}` : `[${el.role}]`;
}

const BOX_COLOR = '#ff3b30'; // red outline + number tag, high contrast over most UIs
const TAG_TEXT_COLOR = '#ffffff';
const FONT = 'bold 16px sans-serif';
const TAG_HEIGHT = 24;
const TAG_PAD_X = 6;

/**
 * Paint a 1-based numbered box over each element and return the annotated image plus the
 * mark→element table. This is the core of the "omni scout": it lets the model ground an
 * action to a labeled box (a mark) instead of guessing pixel coordinates.
 */
export function annotate(
  screenshot: Screenshot,
  elements: UiElement[],
  opts: AnnotateOptions = {},
): SoMResult {
  const max = opts.maxElements ?? 50;
  const picks = elements.slice(0, max);

  const canvas = createCanvas(screenshot.width, screenshot.height);
  const ctx = canvas.getContext('2d');

  // Decode the original screenshot as the base layer.
  const img = new Image();
  img.src = screenshot.png;
  ctx.drawImage(img, 0, 0);

  ctx.font = FONT;
  ctx.textBaseline = 'middle';

  const table: SoMElement[] = [];
  picks.forEach((el, i) => {
    const mark = i + 1;
    const { x, y, width, height } = el.bounds;

    // Element outline.
    ctx.strokeStyle = BOX_COLOR;
    ctx.lineWidth = 3;
    ctx.strokeRect(x, y, width, height);

    // Numbered tag in the top-left corner.
    const tagText = String(mark);
    const tagWidth = ctx.measureText(tagText).width + TAG_PAD_X * 2;
    ctx.fillStyle = BOX_COLOR;
    ctx.fillRect(x, y, tagWidth, TAG_HEIGHT);
    ctx.fillStyle = TAG_TEXT_COLOR;
    ctx.fillText(tagText, x + TAG_PAD_X, y + TAG_HEIGHT / 2 + 1);

    table.push({ mark, element: el, description: describeElement(el) });
  });

  return {
    annotated: { width: screenshot.width, height: screenshot.height, png: canvas.toBuffer('image/png') },
    table,
  };
}

/** Render the element table as text for the LLM, e.g. "1: [button] Login\n2: ...". */
export function formatTable(table: SoMElement[]): string {
  return table.map((e) => `${e.mark}: ${e.description}`).join('\n');
}
