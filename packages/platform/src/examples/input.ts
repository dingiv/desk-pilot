/**
 * desktop input example — create ONE {@link DesktopPlatform} and inject keyboard + mouse actions.
 *
 *   pnpm --filter @vrover/platform example:input
 *   pnpm example:input   # from packages/platform/
 *
 * Drives the real input layer ({@link DesktopNativeLayerAdapter} → uinput via the napi `.node`):
 * click, type, key combos, and scroll. Each step is logged and spaced with a short delay so a
 * human can watch it happen. Real injection needs `/dev/uinput` (run `sudo chmod 0666 /dev/uinput`
 * once per session); without it the adapter logs "would …" and the script still completes.
 */
import { DesktopPlatform, DesktopNativeLayerAdapter } from '@vrover/platform';

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

/** Run one labelled action, then pause so the effect is visible on screen. */
async function step(label: string, fn: () => Promise<void>): Promise<void> {
  console.log(`→ ${label}`);
  await fn();
  await sleep(800);
}

// Create the platform ONCE. The adapter opens a single uinput device handle lazily on the first
// action and reuses it for every click/type/scroll/keypress below.
const platform = new DesktopPlatform(new DesktopNativeLayerAdapter());

await step('click (60, 14) — Activities corner (GNOME)', () => platform.performClick(60, 14));
await step('type "calc"', () => platform.performType('calc'));
await step('keypress Return', () => platform.performKeypress('Return'));
await step('keypress ctrl+l — focus address/entry', () => platform.performKeypress('ctrl+l'));
await step('keypress super — open Activities', () => platform.performKeypress('super'));
await step('scroll down at (960, 540)', () => platform.performScroll(960, 540, 'down'));

console.log('done.');
