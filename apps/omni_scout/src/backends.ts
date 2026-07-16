import { CalculatorPlatform, DesktopPlatform, MockPlatform, MultiScreenPlatform } from '@vrover/platform';
import type { Platform } from '@vrover/platform';
import type { HandshakeRequest } from '@vrover/scout-protocol';

/**
 * Backend registry for the Visual Scout server app — maps a backend name to a factory
 * that mints a fresh {@link Platform} (operation terminal) for a new session. Adding a
 * backend = adding one entry here.
 *
 * - `multi-screen` — the default, an in-memory two-screen app (login → home) rendered with
 *   @napi-rs/canvas; the "extended mock" the server drives out of the box.
 * - `calculator` — an in-memory Ubuntu (GNOME) calculator with a real arithmetic core.
 * - `mock` — the single-screen {@link MockPlatform} (the in-process path's backend).
 * - `desktop` — the reserved Rust seam: fails fast from every method until a `NativeLayer`
 *   (napi-rs: xcap/enigo/AT-SPI) is supplied. Listed so `--backend desktop` resolves to the
 *   real stub and surfaces the actionable "not built" error rather than an unknown name.
 */
export interface BackendDef {
  readonly name: string;
  readonly description: string;
  create(): Platform;
}

export const BACKENDS: Readonly<Record<string, BackendDef>> = {
  'multi-screen': {
    name: 'multi-screen',
    description: 'in-memory two-screen app (login → home); default',
    create: () => new MultiScreenPlatform(),
  },
  calculator: {
    name: 'calculator',
    description: 'in-memory Ubuntu calculator (real arithmetic core)',
    create: () => new CalculatorPlatform(),
  },
  mock: {
    name: 'mock',
    description: 'single-screen in-memory mock',
    create: () => new MockPlatform(),
  },
  desktop: {
    name: 'desktop',
    description: 'native desktop (reserved Rust seam; needs the napi-rs module)',
    create: () => new DesktopPlatform(),
  },
};

/** The backend used when neither the client nor the CLI asks for a specific one. */
export const DEFAULT_BACKEND = 'multi-screen';

/** Names of available backends, in declaration order (for the --help listing). */
export const BACKEND_NAMES: readonly string[] = Object.keys(BACKENDS);

/**
 * Resolve which backend to mint for a session. A client may hint a backend in its
 * handshake (`req.backend`); if that names a known backend it wins, otherwise we fall
 * back to the CLI-configured default. Keeps per-session isolation while letting a
 * capable client pick its target.
 */
export function resolveBackend(req: HandshakeRequest | undefined, defaultName: string): BackendDef {
  const hinted = req?.backend ? BACKENDS[req.backend] : undefined;
  return hinted ?? BACKENDS[defaultName] ?? BACKENDS[DEFAULT_BACKEND]!;
}
