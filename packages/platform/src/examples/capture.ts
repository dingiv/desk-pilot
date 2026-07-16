/**
 * desktop capture example — create ONE {@link DesktopPlatform} and grab a screenshot.
 *
 *   pnpm --filter @vrover/platform example:capture [output.png]
 *   pnpm example:capture [output.png]   # from packages/platform/
 *
 * The platform is built from the real native layer ({@link DesktopNativeLayerAdapter} → in-process
 * PipeWire capture via the napi `.node`, with a `capture_one` binary / X11 / placeholder fallback
 * chain). The single frame is written to disk (default `./vrover-capture.png`) and its dimensions
 * printed. A graphical session yields a real frame; the fallback chain means this still runs
 * (with a placeholder) in a headless container.
 */
import { writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { DesktopPlatform, DesktopNativeLayerAdapter } from '@vrover/platform';

const out = resolve(process.cwd(), process.argv[2] ?? 'vrover-capture.png');

// Create the platform ONCE. The adapter negotiates the PipeWire ScreenCast session lazily on the
// first capture (≈seconds) and reuses it afterwards (a second frame is ~ms), so hold onto this
// instance for every grab rather than reconstructing it per call.
const platform = new DesktopPlatform(new DesktopNativeLayerAdapter());

console.log('capturing…');
const shot = await platform.captureScreen();
writeFileSync(out, shot.png);
console.log(`saved ${shot.width}×${shot.height} screenshot → ${out}`);
