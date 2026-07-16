/**
 * @vrover/platform — VRover's unified target abstraction.
 *
 * The {@link Platform} interface (capture / elements / click / type / scroll / keypress) plus
 * the in-memory implementations (`MockPlatform`, `MultiScreenPlatform`,
 * `CalculatorPlatform`) and the reserved Rust seam (`DesktopPlatform`/`NativeLayer`).
 * `UiElement`/`Bounds` are re-exported from `@vrover/scout-protocol` (their canonical home).
 * Adding a target = adding an implementation.
 */
export type { Bounds, UiElement, Screenshot, Action, ActionResult, GroundingSource, Platform } from './types.js';
export { centerOf, contains } from './types.js';
export { MockPlatform } from './mock/index.js';
export { CalculatorPlatform } from './mock/index.js';
export { MultiScreenPlatform } from './multi-screen.js';
export { DesktopPlatform } from './desktop.js';
export type { NativeLayer } from './desktop.js';
export { DesktopNativeLayerAdapter } from './node-native-layer.js';
