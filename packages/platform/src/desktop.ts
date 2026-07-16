import type { Platform, Screenshot, UiElement } from './types.js';

/**
 * The reserved **Rust seam**.
 *
 * The "high-performance parts in Rust" plan (design.md) is: a native napi-rs module
 * exposes low-level desktop primitives — screen capture (xcap), mouse/keyboard
 * injection (enigo), and the accessibility tree (AT-SPI) for tier-1 grounding.
 * {@link NativeLayer} is the TypeScript contract that future module will satisfy;
 * {@link DesktopPlatform} assembles those primitives into the {@link Platform} the
 * rest of VRover talks to.
 *
 * Nothing here is wired to a real native module yet — there is no Rust build in this
 * iteration (TS-only, per the agreed plan). Until a `NativeLayer` is supplied,
 * `DesktopPlatform` throws a clear, actionable error from every method rather than
 * silently no-op'ing. When the napi-rs module lands, it implements `NativeLayer` and
 * `DesktopPlatform` starts working with no other changes.
 */
export interface NativeLayer {
  captureScreen(): Promise<Screenshot>;
  performClick(x: number, y: number): Promise<void>;
  performType(text: string): Promise<void>;
  performScroll(x: number, y: number, direction: 'up' | 'down'): Promise<void>;
  performKeypress(keys: string): Promise<void>;
  /** Accessibility-tree elements (AT-SPI on Linux) — tier-1 grounding source. */
  getAccessibilityElements(): Promise<UiElement[]>;
}

const NOT_AVAILABLE =
  'Native desktop backend is not available: the Rust (napi-rs) module is not built. ' +
  'Use the mock/browser backend, or build the native module and pass a NativeLayer to DesktopPlatform.';

/**
 * Desktop {@link Platform} backed by a {@link NativeLayer}. Without one, every call
 * fails fast with a message pointing at the missing native module.
 */
export class DesktopPlatform implements Platform {
  constructor(private readonly native?: NativeLayer) {}

  private require(): NativeLayer {
    if (!this.native) throw new Error(NOT_AVAILABLE);
    return this.native;
  }

  async captureScreen(): Promise<Screenshot> {
    return this.require().captureScreen();
  }

  async getElements(): Promise<UiElement[]> {
    return this.require().getAccessibilityElements();
  }

  async performClick(x: number, y: number): Promise<void> {
    await this.require().performClick(x, y);
  }

  async performType(text: string): Promise<void> {
    await this.require().performType(text);
  }

  async performScroll(x: number, y: number, direction: 'up' | 'down'): Promise<void> {
    await this.require().performScroll(x, y, direction);
  }

  async performKeypress(keys: string): Promise<void> {
    await this.require().performKeypress(keys);
  }
}
