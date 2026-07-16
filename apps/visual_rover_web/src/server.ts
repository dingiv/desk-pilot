/**
 * visual-rover-web — the VRover GUI agent (brain) with a React web frontend.
 *
 * One koa process serves BOTH the API (`/api/*`) and the SPA. In dev (NODE_ENV != production)
 * it mounts Vite's dev middleware (HMR); in prod it serves the built `web-dist/` statically
 * with an SPA fallback.
 *
 *   NODE_ENV=development tsx src/server.ts        # dev (Vite middleware + HMR)
 *   NODE_ENV=production  tsx src/server.ts        # prod (serve web-dist/, run `vite build` first)
 *
 * The dev/prod switch on the SERVER is `process.env.NODE_ENV`. The CLIENT distinguishes dev/prod
 * via Vite's `import.meta.env.DEV` / `.MODE` (statically replaced at build time — server-side code
 * can't read it). `NODE_ENV` is the single switch both Vite and koa honour.
 *
 * ## Layers
 *
 *   routes.ts   ← 分发层 (HTTP dispatch, request parse, SSE handshake)
 *   service.ts  ← 业务层 (agent orchestration, task lifecycle)
 *   store.ts    ← 持久层 (TaskStore interface + MemoryTaskStore)
 *   agent.ts    ← LLM / platform adapter to @vrover/agent
 *   server.ts   ← this file: CLI + wiring + process lifecycle
 */
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { readFile, stat } from 'node:fs/promises';
import { parseArgs } from 'node:util';
import Koa from 'koa';
import { createServer as createViteServer, type ViteDevServer } from 'vite';
import { loadConfig } from '@vrover/config';
import type { VroverConfig } from '@vrover/config';
import type { Platform } from '@vrover/platform';
import { createLogger, Logger } from '@vrover/logger';
import { AgentService } from './service.js';
import { MemoryTaskStore } from './store.js';
import { createRoutes } from './routes.js';
import { createPlatform, PLATFORM_NAMES } from './agent.js';
import type { PlatformName } from './agent.js';

// Lifecycle/diagnostic output goes through the unified logger; CLI help (--help), the startup
// banner, and arg-validation errors stay raw `console` (conventional, formatted user output).

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const APP_DIR = path.resolve(__dirname, '..');
const WEB_DIST = path.join(APP_DIR, 'web-dist');
const VITE_CONFIG = path.join(APP_DIR, 'vite.config.ts');

/** Server dev/prod switch. `import.meta.env` is client-only; NODE_ENV is the shared signal. */
const isDev = process.env.NODE_ENV !== 'production';

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.css': 'text/css',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.map': 'application/json',
};

const USAGE = `\
visual-rover-web — VRover GUI agent (brain) + React web UI

Usage:
  NODE_ENV=development tsx src/server.ts [options]   # dev (Vite middleware + HMR)
  NODE_ENV=production  tsx src/server.ts [options]   # prod (serve web-dist/)

Options:
  --host <host>       Listen host (default: 127.0.0.1)
  --port <port>       Listen port (default: 8080)
  --platform <p>      mock | remote | desktop (default: mock)
  --scout-host <host> Scout server host (remote platform; default: from config / env)
  --scout-port <port> Scout server port (remote platform; default: from config / env)
  --max-steps <n>     Max agent steps per task (default: from config)
  -h, --help          Show this help and exit`;

interface ServerOptions {
  host: string;
  port: number;
  maxSteps?: number;
  /** Resolved target platform (one per server; selected at boot). */
  platform: Platform;
  /** Backend name, for logging. */
  platformName: PlatformName;
  logger: Logger;
}

interface WebHandle {
  host: string;
  port: number;
  close(): Promise<void>;
}

async function startServer(opts: ServerOptions): Promise<WebHandle> {
  // ── wire layers: store → service → routes ───────────────────────────────
  const store = new MemoryTaskStore();
  const service = new AgentService(store, opts.platform);
  // Hold the logger itself, not a destructured method — `logger.info` stays bound regardless of
  // how the Logger is implemented (closures today, but this is robust to a `this`-based rewrite).
  const logger = opts.logger;
  const routes = createRoutes(service, isDev);

  const app = new Koa();
  app.use(routes);

  // ── SPA: dev = Vite middleware (HMR); prod = static + SPA fallback ──────
  let vite: ViteDevServer | undefined;
  if (isDev) {
    app.use(async (ctx) => {
      if (!vite) {
        ctx.status = 503;
        ctx.body = 'dev server is starting…';
        return;
      }
      await new Promise<void>((resolve, reject) => {
        vite!.middlewares(ctx.req, ctx.res, (err: unknown) =>
          err instanceof Error ? reject(err) : resolve(),
        );
      });
    });
  } else {
    app.use(async (ctx) => {
      await serveStatic(ctx, WEB_DIST);
    });
  }

  const httpServer: http.Server = app.listen(opts.port, opts.host);

  if (isDev) {
    vite = await createViteServer({
      root: APP_DIR,
      configFile: VITE_CONFIG,
      server: { middlewareMode: true, hmr: { server: httpServer } },
    });
  }

  logger.info(`VRover web server listening on http://${opts.host}:${opts.port} (${isDev ? 'dev' : 'prod'})`);
  logger.info(`  platform: ${opts.platformName}`);
  if (isDev) logger.info(`  Vite middleware + HMR (root: ${APP_DIR})`);
  else logger.info(`  serving ${WEB_DIST}`);

  return {
    host: opts.host,
    port: opts.port,
    close: async () => {
      if (vite) await vite.close();
      await new Promise<void>((resolve) => httpServer.close(() => resolve()));
    },
  };
}

/** Serve a static file from `root`, falling back to the SPA index.html. */
async function serveStatic(ctx: Koa.Context, root: string): Promise<void> {
  const rel = decodeURIComponent(ctx.path);
  const filePath = path.resolve(root, '.' + rel);
  if (!filePath.startsWith(root)) {
    ctx.status = 403;
    return;
  }
  try {
    const s = await stat(filePath);
    const target = s.isDirectory() ? path.join(filePath, 'index.html') : filePath;
    ctx.body = await readFile(target);
    ctx.type = MIME[path.extname(target)] ?? 'application/octet-stream';
  } catch {
    ctx.type = 'html';
    ctx.body = await readFile(path.join(root, 'index.html'));
  }
}

// ── entry ────────────────────────────────────────────────────────────────────

async function main(): Promise<void> {
  const { values } = parseArgs({
    options: {
      host: { type: 'string' },
      port: { type: 'string' },
      platform: { type: 'string' },
      'scout-host': { type: 'string' },
      'scout-port': { type: 'string' },
      'max-steps': { type: 'string' },
      help: { type: 'boolean', short: 'h' },
    },
    args: forwardedArgs(),
    strict: true,
  });

  if (values.help) {
    console.log(USAGE);
    return;
  }

  const host = values.host ?? '127.0.0.1';
  const port = parsePort(values.port ?? '8080');
  const maxSteps = values['max-steps'] ? parseUint(values['max-steps'], '--max-steps') : undefined;
  const logger = createLogger('web/server');

  const platformName = parsePlatformName(values.platform);
  const cfg = loadConfig(buildScoutOverrides(values));
  const platform = createPlatform(platformName, cfg);

  if (!isDev) {
    try {
      await readFile(path.join(WEB_DIST, 'index.html'));
    } catch {
      logger.error(`No built SPA at ${WEB_DIST}. Run \`pnpm --filter @vrover/visual-rover-web build:web\` (vite build) first.`);
      process.exit(1);
    }
  }

  const server = await startServer({
    host,
    port,
    maxSteps,
    platform,
    platformName,
    logger,
  });

  console.log('\n  open  http://%s:%d', server.host, server.port);
  console.log('  POST /api/run  { task: string }   → { result, log }');
  console.log('  GET  /api/run/stream?task=...     → SSE step-by-step');
  console.log('  GET  /api/health                  → liveness probe');
  console.log('\nPress Ctrl+C to stop.');

  const shutdown = async (sig: string) => {
    logger.info(`${sig} received, shutting down…`);
    await server.close();
    process.exit(0);
  };
  process.on('SIGINT', () => void shutdown('SIGINT'));
  process.on('SIGTERM', () => void shutdown('SIGTERM'));
}

// ── helpers ──────────────────────────────────────────────────────────────────

function parsePort(raw: string): number {
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 0 || n > 65535) {
    console.error(`Invalid --port "${raw}" — expected an integer in 0..65535.`);
    process.exit(2);
  }
  return n;
}

function parseUint(raw: string, flag: string): number {
  const n = Number(raw);
  if (!Number.isInteger(n) || n <= 0) {
    console.error(`Invalid ${flag} "${raw}" — expected a positive integer.`);
    process.exit(2);
  }
  return n;
}

/** Resolve + validate `--platform` (default `mock`). */
function parsePlatformName(raw: string | undefined): PlatformName {
  const name = (raw ?? 'mock') as PlatformName;
  if (!PLATFORM_NAMES.includes(name)) {
    console.error(
      `Invalid --platform "${raw}". Use one of: ${PLATFORM_NAMES.join(', ')}.`,
    );
    process.exit(2);
  }
  return name;
}

/** Build config overrides for the Scout server address (remote platform only). */
function buildScoutOverrides(values: Record<string, unknown>): Partial<VroverConfig> {
  const overrides: Record<string, unknown> = {};
  const scoutHost = values['scout-host'] as string | undefined;
  const scoutPort = values['scout-port'] as string | undefined;
  if (scoutHost) setNested(overrides, ['scout', 'host'], scoutHost);
  if (scoutPort) setNested(overrides, ['scout', 'port'], parseUint(scoutPort, '--scout-port'));
  return overrides as Partial<VroverConfig>;
}

function setNested(obj: Record<string, unknown>, path: string[], value: unknown): void {
  let cur = obj;
  for (let i = 0; i < path.length - 1; i++) {
    const k = path[i]!;
    if (!cur[k]) cur[k] = {};
    cur = cur[k] as Record<string, unknown>;
  }
  cur[path[path.length - 1]!] = value;
}

function forwardedArgs(): string[] {
  const args = process.argv.slice(2);
  const sepIdx = args.indexOf('--');
  if (sepIdx >= 0) {
    return [...args.slice(0, sepIdx), ...args.slice(sepIdx + 1)];
  }
  return args;
}

void main();
