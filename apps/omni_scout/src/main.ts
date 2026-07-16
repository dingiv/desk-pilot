/**
 * omni-scout — the standalone Visual Scout server.
 *
 * Runs the UI-operation + grounding service (`@vrover/scout`) as its own process: each
 * connecting client handshakes and gets an isolated session with its own backend
 * terminal. Needs no API key — it only exposes UI operations over the custom TCP
 * protocol.
 *
 *   pnpm --filter @vrover/omni-scout start                                  # defaults (multi-screen on SCOUT_HOST:SCOUT_PORT)
 *   pnpm --filter @vrover/omni-scout start -- --host 0.0.0.0 --port 9000    # bind publicly on 9000
 *   pnpm --filter @vrover/omni-scout start -- --backend mock                # use the single-screen mock
 *   pnpm --filter @vrover/omni-scout start -- --devtools-port 7881          # also expose the browser devtools service
 *
 * Host/port default to `SCOUT_HOST`/`SCOUT_PORT` (then `127.0.0.1` / `7878`), mirroring
 * the server itself. A client may also hint a backend in its handshake; see
 * {@link resolveBackend}.
 */
import { parseArgs } from 'node:util';
import { startScoutServer } from '@vrover/scout';
import { BACKEND_NAMES, DEFAULT_BACKEND, resolveBackend } from './backends.js';

const USAGE = `\
omni-scout — Visual Scout UI-operation server

Usage:
  omni-scout [options]

Options:
  --host <host>       Bind host (default: $SCOUT_HOST or 127.0.0.1)
  --port <port>       Bind port (default: $SCOUT_PORT or 7878; 0 = OS-assigned)
  --backend <name>    Default backend terminal (default: ${DEFAULT_BACKEND})
                      One of: ${BACKEND_NAMES.join(', ')}
                      A client may override this via its handshake 'backend' hint.
  --devtools-port <port>  Also expose the browser devtools HTTP/SSE service (off unless set; 0 = OS-assigned)
  --devtools-host <host>  Devtools bind host (default: 127.0.0.1)
  -h, --help          Show this help and exit

Each connecting client handshakes → gets a fresh session + isolated backend terminal.
Press Ctrl+C to stop. Needs no API key.`;

function main(): void {
  const { values } = parseArgs({
    options: {
      host: { type: 'string' },
      port: { type: 'string' },
      backend: { type: 'string' },
      'devtools-port': { type: 'string' },
      'devtools-host': { type: 'string' },
      help: { type: 'boolean', short: 'h' },
    },
    args: forwardedArgs(),
    strict: true,
  });

  if (values.help) {
    console.log(USAGE);
    return;
  }

  const host = values.host ?? process.env.SCOUT_HOST ?? '127.0.0.1';
  const port = parsePort(values.port ?? process.env.SCOUT_PORT ?? '7878');
  const defaultBackend = values.backend ?? DEFAULT_BACKEND;
  if (!BACKEND_NAMES.includes(defaultBackend)) {
    console.error(`Unknown backend "${defaultBackend}". One of: ${BACKEND_NAMES.join(', ')}.`);
    process.exit(2);
  }

  const devtoolsPort = values['devtools-port'] === undefined ? undefined : parsePort(values['devtools-port']);
  const devtoolsHost = values['devtools-host'];

  void run({ host, port, defaultBackend, devtoolsHost, devtoolsPort });
}

/** Resolve + bind the server, then keep it running until interrupted. */
async function run(opts: {
  host: string;
  port: number;
  defaultBackend: string;
  devtoolsHost?: string;
  devtoolsPort?: number;
}): Promise<void> {
  const { host, port, defaultBackend, devtoolsHost, devtoolsPort } = opts;
  const server = await startScoutServer({
    host,
    port,
    backendFactory: (req) => resolveBackend(req, defaultBackend).create(),
    backendName: defaultBackend,
    devtoolsHost,
    devtoolsPort,
    log: (m) => console.log(m),
  });

  console.log(`\nVisual Scout server ready at ${server.host}:${server.port} (TCP, custom binary protocol)`);
  console.log(`  default backend: ${defaultBackend}  (handshake 'backend' hint can override per client)`);
  if (server.devtools) {
    console.log(`  devtools service: http://${server.devtools.host}:${server.devtools.port}  (open with: pnpm --filter @vrover/omni-scout-devtools dev -- --api http://${server.devtools.host}:${server.devtools.port})`);
  }
  console.log('\nPress Ctrl+C to stop.');

  process.on('SIGINT', async () => {
    console.log('\nShutting down…');
    await server.close();
    process.exit(0);
  });
  process.on('SIGTERM', async () => {
    await server.close();
    process.exit(0);
  });
}

/** Parse a port arg/env value; `0` is allowed (OS-assigned, handy for tests). */
function parsePort(raw: string): number {
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 0 || n > 65535) {
    console.error(`Invalid port "${raw}" — expected an integer in 0..65535.`);
    process.exit(2);
  }
  return n;
}

/** `pnpm --filter @vrover/omni-scout start -- <args>` forwards a literal `--`; drop a leading one so flags parse. */
function forwardedArgs(): string[] {
  const args = process.argv.slice(2);
  return args.length > 0 && args[0] === '--' ? args.slice(1) : args;
}

main();
