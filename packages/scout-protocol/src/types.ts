/**
 * Wire-level domain types carried by the Scout protocol — the contract shared by
 * the server (`@vrover/scout`), the standalone client SDK (`@vrover/scout-client`),
 * and the platform layer (`@vrover/platform`, which re-exports `UiElement`/`Bounds`).
 *
 * These live here (not in `@vrover/platform`) so the client SDK can surface them
 * while depending on the protocol *only* — never on the rest of the project.
 */

/** An axis-aligned rectangle in screen pixels. */
export interface Bounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** A single UI element carried over the wire (a Set-of-Mark candidate). */
export interface UiElement {
  id: string;
  /** Semantic role, e.g. 'button' | 'input' | 'link' | 'text' | 'window'. */
  role: string;
  /** Accessible name / visible label. */
  label: string;
  bounds: Bounds;
  /** Extra, target-specific info (DOM tag, AT-SPI state, etc.). */
  attributes?: Record<string, unknown>;
}

/** A decoded screenshot: dimensions + raw PNG bytes. */
export interface CaptureResult {
  width: number;
  height: number;
  png: Buffer;
}
