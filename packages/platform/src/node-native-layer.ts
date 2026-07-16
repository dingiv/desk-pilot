/**
 * Desktop {@link NativeLayer} backed by Node.js primitives + the napi-rs Rust binding.
 *
 * Screen capture tries X11 (`import -window root`) first; falls back to a placeholder
 * when no display server is available (headless containers / CI).
 *
 * Input injection bridges to the napi-rs
 * {@link import('@vrover/native').DesktopNativeLayer} (Linux uinput). When uinput is
 * unavailable (no `/dev/uinput`), actions are logged instead — the agent loop still
 * completes for demo / CI purposes, and real input works on a physical desktop.
 *
 * Accessibility is a placeholder (empty array) until AT-SPI is wired.
 */
import { execSync, execFileSync } from 'node:child_process';
import { existsSync, readFileSync, statSync, unlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import type { Screenshot, UiElement } from './types.js';
import type { NativeLayer } from './desktop.js';
import { createLogger } from '@vrover/logger';

const require = createRequire(import.meta.url);

const logger = createLogger('platform/desktop');

// ── placeholder PNG (1×1 pixel, grey) ────────────────────────────────────────
// Minimal valid PNG for when no display server is available.
// Generated once, reused — no runtime deps.
let _placeholderPng: Buffer | undefined;

function placeholderPng(): Buffer {
  if (_placeholderPng) return _placeholderPng;
  // Build a minimal 1×1 grey PNG manually.
  // Structure: sig | IHDR | IDAT | IEND
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]); // PNG signature

  // IHDR: 1×1, 8-bit RGB
  const ihdrData = Buffer.alloc(13);
  ihdrData.writeUInt32BE(1, 0); // width
  ihdrData.writeUInt32BE(1, 4); // height
  ihdrData[8] = 8;  // bit depth
  ihdrData[9] = 2;  // color type (RGB)
  ihdrData[10] = 0; // compression
  ihdrData[11] = 0; // filter
  ihdrData[12] = 0; // interlace
  const ihdr = chunk('IHDR', ihdrData);

  // IDAT: raw pixel data (filter byte 0x00 + RGB 128,128,128), zlib-compressed
  const raw = Buffer.from([0x00, 128, 128, 128]); // filter=none, grey pixel
  // Minimal zlib stream (no compression, stored block):
  // CMF=0x78 FLG=0x01 → header; stored block: 0x01 (final), len(2)+nlen(2), data, adler32(4)
  const adler = adler32(raw);
  const stored = Buffer.alloc(4 + raw.length + 4);
  stored[0] = 0x00; // final block flag + stored type
  stored[1] = raw.length & 0xff;
  // raw.length is 4, so len=4, nlen=0xfffb
  stored.writeUInt16LE(raw.length, 1);
  stored.writeUInt16LE(raw.length ^ 0xffff, 3);
  raw.copy(stored, 5);
  stored.writeUInt32BE(adler, 5 + raw.length);
  // zlib header + stored block
  const zlibData = Buffer.concat([Buffer.from([0x78, 0x01]), stored]);
  const idat = chunk('IDAT', zlibData);

  const iend = chunk('IEND', Buffer.alloc(0));

  _placeholderPng = Buffer.concat([sig, ihdr, idat, iend]);
  return _placeholderPng;
}

function chunk(type: string, data: Buffer): Buffer {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const typeB = Buffer.from(type, 'ascii');
  const crc = crc32(Buffer.concat([typeB, data]));
  const crcB = Buffer.alloc(4);
  crcB.writeUInt32BE(crc, 0);
  return Buffer.concat([len, typeB, data, crcB]);
}

// CRC-32 (PNG uses standard CRC-32)
function crc32(buf: Buffer): number {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) {
    c = CRC_TABLE[(c ^ buf[i]!) & 0xff]! ^ (c >>> 8);
  }
  return (c ^ 0xffffffff) >>> 0;
}

const CRC_TABLE: Uint32Array = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    t[n] = c;
  }
  return t;
})();

// Adler-32
function adler32(buf: Buffer): number {
  let a = 1, b = 0;
  for (let i = 0; i < buf.length; i++) {
    a = (a + buf[i]!) % 65521;
    b = (b + a) % 65521;
  }
  return ((b << 16) | a) >>> 0;
}

// ── screen capture ────────────────────────────────────────────────────────────

// PipeWire is the working path on a GNOME/Wayland desktop: `import -window root`
// (X11) has no xauth here and would capture a black XWayland root anyway. We shell
// out to the Rust `capture_one` example (crates/drivers), which negotiates a
// ScreenCast session over the xdg-desktop-portal and mmap's a real BGRx frame.
// Override the binary with VROVER_CAPTURE_BIN; otherwise look it up under the
// workspace's target/debug/examples/.

function captureBinaryPath(): string | null {
  const fromEnv = process.env.VROVER_CAPTURE_BIN;
  if (fromEnv) return existsSync(fromEnv) ? fromEnv : null;
  // packages/platform/src/node-native-layer.ts → workspace root is 3 levels up.
  const here = dirname(fileURLToPath(import.meta.url));
  const root = resolve(here, '../../..');
  const candidate = join(root, 'target/debug/examples/capture_one');
  return existsSync(candidate) ? candidate : null;
}

/** The portal needs the session bus; derive it from XDG_RUNTIME_DIR if unset. */
function dbusEnv(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  if (!env.DBUS_SESSION_BUS_ADDRESS && env.XDG_RUNTIME_DIR) {
    env.DBUS_SESSION_BUS_ADDRESS = `unix:path=${env.XDG_RUNTIME_DIR}/bus`;
  }
  return env;
}

function tryPipewireCapture(): Buffer | null {
  const bin = captureBinaryPath();
  if (!bin) return null;
  const out = join(tmpdir(), `vrover-capture-${randomUUID()}.png`);
  try {
    execFileSync(bin, [out], {
      env: dbusEnv(),
      timeout: 30_000,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return readFileSync(out);
  } catch {
    return null;
  } finally {
    try { unlinkSync(out); } catch { /* temp already gone */ }
  }
}

function tryX11Capture(): Buffer | null {
  try {
    const env: Record<string, string | undefined> = { ...process.env };
    // Some setups keep Xauthority at non-standard paths
    if (!env.XAUTHORITY) {
      const candidates = [
        `${process.env.HOME}/.Xauthority`,
        `/tmp/xauth_${process.env.USER}`,
      ];
      for (const c of candidates) {
        try {
          const s = statSync(c, { throwIfNoEntry: false });
          if (s && s.size > 0) {
            env.XAUTHORITY = c;
            break;
          }
        } catch { /* ignore */ }
      }
    }
    return execSync('import -window root png:-', {
      env,
      timeout: 10_000,
      stdio: ['ignore', 'pipe', 'pipe'],
      encoding: undefined, // return Buffer
    });
  } catch {
    return null;
  }
}

function captureFallback(): Screenshot {
  // ImageMagick `convert` doesn't need X11 — try it first for a proper-sized placeholder
  try {
    const png = execSync(
      'convert -size 1920x1080 xc:"#808080" png:-',
      { timeout: 5_000, stdio: ['ignore', 'pipe', 'pipe'], encoding: undefined },
    );
    return { width: 1920, height: 1080, png };
  } catch {
    // Last resort: tiny 1×1 PNG
    const png = placeholderPng();
    return { width: 1, height: 1, png };
  }
}

// ── input bridge ──────────────────────────────────────────────────────────────

interface InputSink {
  click(x: number, y: number, button: string): void;
  typeText(text: string): void;
  scroll(x: number, y: number, dx: number, dy: number): void;
  tapKey(key: string): void;
  keyPress(key: string): void;
  keyRelease(key: string): void;
}

interface ScreenCapture {
  /** Capture the latest frame as a PNG Buffer (blocks until first frame). */
  captureScreen(timeoutMs?: number): Buffer;
}

function tryCreateInputSink(): InputSink | null {
  try {
    // Dynamic import of the napi binding — throws if .node missing or uinput
    // unavailable (constructor opens /dev/uinput).
    const { createDesktopLayer } = require('@vrover/native');
    return createDesktopLayer();
  } catch {
    return null;
  }
}

function tryCreateCapture(): ScreenCapture | null {
  try {
    // Throws if the .node was built without the `capture` feature (binding has
    // no DesktopCapture) or the ScreenCast portal can't be negotiated.
    const { createCapture } = require('@vrover/native');
    return createCapture();
  } catch {
    return null;
  }
}

// ── adapter ───────────────────────────────────────────────────────────────────

export class DesktopNativeLayerAdapter implements NativeLayer {
  private _input: InputSink | null | undefined; // undefined = not tried yet
  private _capture: ScreenCapture | null | undefined; // undefined = not tried yet

  private input(): InputSink | null {
    if (this._input === undefined) {
      this._input = tryCreateInputSink();
      if (!this._input) {
        logger.warn(
          'Native input not available (no /dev/uinput?). Actions will be logged but not executed.',
        );
      }
    }
    return this._input;
  }

  private capture(): ScreenCapture | null {
    if (this._capture === undefined) {
      this._capture = tryCreateCapture();
      if (!this._capture) {
        logger.warn(
          'Native capture not available (.node built without the `capture` feature, or no ' +
            'graphical session). Falling back to the capture_one binary / X11.',
        );
      }
    }
    return this._capture;
  }

  // ── capture ──────────────────────────────────────────────────────────────

  async captureScreen(): Promise<Screenshot> {
    // 1. napi PipeWire capture (in-process, dialog-free GNOME/Wayland path).
    const cap = this.capture();
    if (cap) {
      try {
        const png = cap.captureScreen(30_000);
        const { width, height } = readPngSize(png);
        return { width, height, png };
      } catch (e) {
        logger.warn('napi capture failed, falling back:', e instanceof Error ? e.message : e);
      }
    }
    // 2. capture_one binary (PipeWire) — proven one-shot fallback.
    const pw = tryPipewireCapture();
    if (pw) {
      const { width, height } = readPngSize(pw);
      return { width, height, png: pw };
    }
    // 3. X11 `import` — works on a real X session.
    const x = tryX11Capture();
    if (x) {
      const { width, height } = readPngSize(x);
      return { width, height, png: x };
    }
    logger.error('Screen capture not available — using placeholder image.');
    return captureFallback();
  }

  // ── input ────────────────────────────────────────────────────────────────

  async performClick(x: number, y: number): Promise<void> {
    if (this.input()) {
      this.input()!.click(Math.round(x), Math.round(y), 'left');
    } else {
      logger.info(`would click at (${Math.round(x)}, ${Math.round(y)})`);
    }
  }

  async performType(text: string): Promise<void> {
    if (this.input()) {
      this.input()!.typeText(text);
    } else {
      logger.info(`would type: "${text}"`);
    }
  }

  async performScroll(
    x: number,
    y: number,
    direction: 'up' | 'down',
  ): Promise<void> {
    const dy = direction === 'up' ? -3 : 3;
    if (this.input()) {
      this.input()!.scroll(Math.round(x), Math.round(y), 0, dy);
    } else {
      logger.info(`would scroll ${direction} at (${Math.round(x)}, ${Math.round(y)})`);
    }
  }

  async performKeypress(keys: string): Promise<void> {
    if (this.input()) {
      const sink = this.input()!;
      // Simple key (no modifier): "Return", "a", "F1", "escape"
      // Combo: "ctrl+s", "alt+F4"
      const parts = keys.split('+');
      if (parts.length === 1) {
        sink.tapKey(normalizeKey(parts[0]!));
      } else {
        // Press modifiers, tap the final key, release modifiers in reverse
        const mods = parts.slice(0, -1);
        const finalKey = parts[parts.length - 1]!;
        for (const m of mods) {
          sink.keyPress(normalizeKey(m.trim()));
        }
        sink.tapKey(normalizeKey(finalKey));
        for (const m of mods.reverse()) {
          sink.keyRelease(normalizeKey(m.trim()));
        }
      }
    } else {
      logger.info(`would press: "${keys}"`);
    }
  }

  // ── accessibility ────────────────────────────────────────────────────────

  async getAccessibilityElements(): Promise<UiElement[]> {
    // AT-SPI not wired yet — the agent falls back to TS SoM or native OmniParser.
    return [];
  }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/** Map VRover key names → Rust key names where they differ. */
function normalizeKey(key: string): string {
  const m: Record<string, string> = {
    return: 'enter',
    Return: 'enter',
    backspace: 'backspace',
    Backspace: 'backspace',
    escape: 'escape',
    Escape: 'escape',
    esc: 'escape',
    space: 'space',
    Space: 'space',
    tab: 'tab',
    Tab: 'tab',
    delete: 'delete',
    Delete: 'delete',
    insert: 'insert',
    Insert: 'insert',
    up: 'up',
    Up: 'up',
    down: 'down',
    Down: 'down',
    left: 'left',
    Left: 'left',
    right: 'right',
    Right: 'right',
    ctrl: 'ctrl',
    alt: 'alt',
    shift: 'shift',
    super: 'super',
    win: 'super',
    pageup: 'pageup',
    PageUp: 'pageup',
    pagedown: 'pagedown',
    PageDown: 'pagedown',
    home: 'home',
    Home: 'home',
    end: 'end',
    End: 'end',
  };
  return m[key] ?? key;
}

/** Read width/height from a PNG buffer (bytes 16-23). */
function readPngSize(png: Buffer): { width: number; height: number } {
  if (png.length < 24) return { width: 0, height: 0 };
  return {
    width: png.readUInt32BE(16),
    height: png.readUInt32BE(20),
  };
}
