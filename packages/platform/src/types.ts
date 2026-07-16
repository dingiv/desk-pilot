import type { Bounds, UiElement } from '@vrover/scout-protocol';

// Re-export the wire-level domain types (canonical home is @vrover/scout-protocol) so
// downstream packages keep importing them from @vrover/platform as before.
export type { Bounds, UiElement } from '@vrover/scout-protocol';

/**
 * Core platform abstractions.
 *
 * A {@link Platform} is VRover's unified interface to *something the agent can see and act on*.
 * Today the in-memory mocks exist; planned implementations include a native OS platform via a
 * Rust core (xcap/enigo/AT-SPI) and a browser platform (Playwright).
 *
 * The agent never touches the OS/browser directly — it always goes through Platform, so the
 * observe→think→act loop stays identical across targets. Adding a target = adding one package.
 */

/** A raw screenshot image. */
export interface Screenshot {
  width: number;
  height: number;
  /** PNG bytes. Fed to the LLM as an image content block. */
  png: Buffer;
}

/**
 * Actions the agent can request. Elements are referenced by SoM **mark number**, never raw
 * coordinates — the tool executor resolves mark → element → center. This is what keeps the
 * "Set-of-Mark" design grounded: the model picks a labeled box, not a pixel.
 */
export type Action =
  | { type: 'click'; mark: number }
  | { type: 'doubleClick'; mark: number }
  | { type: 'type'; mark: number; text: string }
  | { type: 'scroll'; mark: number; direction: 'up' | 'down' }
  | { type: 'keypress'; keys: string }
  | { type: 'wait' }
  | { type: 'done'; summary: string };

/** Result of executing one action; fed back to the LLM as a tool_result. */
export interface ActionResult {
  ok: boolean;
  /** Human-readable status line for the LLM, e.g. "clicked [button] Login". */
  message: string;
  /** Set when this action completes the task (e.g. login succeeded). */
  finished?: boolean;
}

/**
 * The unified surface the agent drives. Primitives are coordinate-oriented to stay close to
 * real mouse/keyboard input; mark → coordinate resolution lives in the tool executor.
 */
export interface Platform {
  /** Capture the current screen (or page) as a PNG. */
  captureScreen(): Promise<Screenshot>;
  /**
   * Return the UI elements available for SoM annotation right now. Today this is the mock's
   * synthetic elements; later it comes from a {@link GroundingSource} (AT-SPI / DOM / ML vision).
   */
  getElements(): Promise<UiElement[]>;
  /** Click at screen coordinates. */
  performClick(x: number, y: number): Promise<void>;
  /** Type text into whatever currently has focus. */
  performType(text: string): Promise<void>;
  /** Scroll at the given coordinates. */
  performScroll(x: number, y: number, direction: 'up' | 'down'): Promise<void>;
  /** Press a key combo, e.g. "Return", "ctrl+s". */
  performKeypress(keys: string): Promise<void>;
}

/**
 * Source of UI elements for SoM grounding. (Seam for future work — not wired today.)
 *
 * The plan is to combine two kinds of source:
 *   - accessibility / DOM boxes — precise, no ML: AT-SPI on desktop, Playwright DOM in browser;
 *   - ML vision detection (OmniParser-style, onnxruntime) — catches custom-drawn controls the
 *     accessibility tree misses.
 * Today {@link Platform.getElements} stands in for this interface.
 */
export interface GroundingSource {
  detect(): Promise<UiElement[]>;
}

/** Center point of a bounds — turns a mark into a clickable coordinate. */
export function centerOf(bounds: Bounds): { x: number; y: number } {
  return { x: bounds.x + bounds.width / 2, y: bounds.y + bounds.height / 2 };
}

/** True when (x, y) falls inside bounds — used by the mock for hit testing. */
export function contains(bounds: Bounds, x: number, y: number): boolean {
  return (
    x >= bounds.x &&
    x <= bounds.x + bounds.width &&
    y >= bounds.y &&
    y <= bounds.y + bounds.height
  );
}
