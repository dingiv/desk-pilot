import net from 'node:net';
import type { Platform } from '@vrover/platform';
import {
  FrameDecoder,
  MsgType,
  VERSION,
  encodeError,
  encodeFrame,
  encodeJsonFrame,
  parseHandshakeRequest,
  parseRequest,
  type Frame,
  type HandshakeRequest,
} from '@vrover/scout-protocol';
import { GraphMap } from './graph-map.js';
import { Session } from './session.js';
import { startDevtoolsServer, type DevtoolsServer } from './devtools.js';

/**
 * The Visual Scout TCP server. A standalone process that exposes UI-operation +
 * grounding services over a custom binary protocol (see `@vrover/scout-protocol`), with
 * **per-connection sessions** — the "胖工具" component of VRover, in its
 * multi-session server form (D10, pulled forward from M3).
 *
 * Built on `node:net` with no dependencies. The server intentionally does **not**
 * read `loadConfig()` — it must run as a standalone process without an Anthropic
 * key, so host/port come straight from the environment.
 *
 * ## State
 * - **Server-level**: the session registry (`sessions`) + the shared, persistent
 *   {@link GraphMap} (one per app; placeholder today).
 * - **Session-level**: each accepted connection becomes a {@link Session} that
 *   owns its own operation terminal (a {@link Platform} = capture + keyboard +
 *   mouse) and a {@link Session#walker}. A session is created on a successful
 *   handshake and torn down when the socket closes.
 */

export interface ScoutServerOptions {
  /**
   * Mints a fresh {@link Platform} (operation terminal) for each new session,
   * based on the client's handshake requirements. Per-session backends keep
   * clients isolated — one client's login never affects another's.
   */
  backendFactory: (req: HandshakeRequest) => Platform | Promise<Platform>;
  /** Backend name reported in the handshake ack. Defaults to the backend's class name. */
  backendName?: string;
  /** Bind host. Default `SCOUT_HOST` env or `127.0.0.1`. */
  host?: string;
  /** Bind port. Default `SCOUT_PORT` env or `7878`. `0` = OS-assigned (for tests). */
  port?: number;
  /**
   * Also start the browser-facing **devtools** HTTP/SSE service on this port
   * (`0` = OS-assigned). Omit to disable. Shares the session registry with the TCP server.
   */
  devtoolsPort?: number;
  /** Devtools bind host (default `127.0.0.1`). */
  devtoolsHost?: string;
  /** Devtools SSE capture tick rate, ms (default 1000; runtime-tunable via `PUT /api/config`). */
  devtoolsCaptureIntervalMs?: number;
  /** Progress sink; defaults to no-op. */
  log?: (message: string) => void;
}

export interface ScoutServer {
  /** Host the server is listening on. */
  readonly host: string;
  /** Port the server is listening on (OS-assigned when requested as `0`). */
  readonly port: number;
  /** Number of sessions currently open. */
  readonly sessionCount: number;
  /** The browser-facing devtools service, if `devtoolsPort` was set. */
  readonly devtools?: { host: string; port: number };
  /** Stop listening and close all connections. Resolves once the server is closed. */
  close(): Promise<void>;
}

interface ServerCtx {
  backendFactory: (req: HandshakeRequest) => Platform | Promise<Platform>;
  backendName?: string;
  sessions: Map<string, Session>;
  graphMap: GraphMap;
  sockets: Set<net.Socket>;
  seq: number;
  log: (message: string) => void;
}

/** Start the Scout server. Resolves with a handle once it is listening. */
export function startScoutServer(opts: ScoutServerOptions): Promise<ScoutServer> {
  const host = opts.host ?? process.env.SCOUT_HOST ?? '127.0.0.1';
  const port = opts.port ?? Number(process.env.SCOUT_PORT ?? 7878);
  const log = opts.log ?? (() => {});
  const ctx: ServerCtx = {
    backendFactory: opts.backendFactory,
    backendName: opts.backendName,
    sessions: new Map(),
    graphMap: new GraphMap(),
    sockets: new Set(),
    seq: 0,
    log,
  };

  const server = net.createServer((socket) => handleConnection(socket, ctx));

  return new Promise<ScoutServer>((resolve, reject) => {
    server.on('error', reject);
    server.listen(port, host, () => {
      const addr = server.address();
      const actualPort = typeof addr === 'object' && addr ? addr.port : port;
      log(`Visual Scout server listening on ${host}:${actualPort}`);

      const finalize = (devtools?: DevtoolsServer): void => {
        resolve({
          host,
          port: actualPort,
          get sessionCount() {
            return ctx.sessions.size;
          },
          devtools: devtools ? { host: devtools.host, port: devtools.port } : undefined,
          close: () =>
            new Promise<void>((resolveClose) => {
              // Force-close live sockets so close() resolves promptly (test teardown).
              for (const sock of ctx.sockets) sock.destroy();
              ctx.sockets.clear();
              void devtools?.close().catch(() => {});
              server.close(() => resolveClose());
            }),
        });
      };

      if (opts.devtoolsPort !== undefined) {
        startDevtoolsServer(ctx, {
          host: opts.devtoolsHost,
          port: opts.devtoolsPort,
          captureIntervalMs: opts.devtoolsCaptureIntervalMs,
          log,
        }).then(finalize, reject);
      } else {
        finalize(undefined);
      }
    });
  });
}

/** Per-connection state machine: handshake → session → request dispatch. */
function handleConnection(socket: net.Socket, ctx: ServerCtx): void {
  ctx.sockets.add(socket);
  const decoder = new FrameDecoder();
  let session: Session | undefined;
  let handshook = false;

  const fail = (id: number, message: string): void => {
    send(socket, encodeError(id, message));
    socket.destroy();
  };

  socket.on('data', (chunk: Buffer) => {
    let frames: Frame[];
    try {
      frames = decoder.push(chunk);
    } catch (err) {
      return fail(0, errMsg(err)); // malformed framing → terminate
    }

    for (const frame of frames) {
      if (!handshook) {
        if (frame.type !== MsgType.HAND_SHAKE) {
          return fail(0, 'first frame must be HAND_SHAKE');
        }
        // Mint the session from the client's requirements; reply async when ready.
        let req: HandshakeRequest;
        try {
          req = parseHandshakeRequest(frame);
        } catch (err) {
          return fail(0, errMsg(err));
        }
        Promise.resolve(ctx.backendFactory(req))
          .then((backend) => {
            const id = `s_${(++ctx.seq).toString(36)}`;
            session = new Session(id, backend);
            ctx.sessions.set(id, session);
            handshook = true;
            const name = ctx.backendName ?? backend.constructor.name;
            send(socket, encodeJsonFrame(MsgType.HAND_SHAKE_ACK, 0, { sessionId: id, version: VERSION, backend: name }));
            ctx.log(`session ${id} ready (backend: ${name})`);
          })
          .catch((err) => fail(0, `handshake failed: ${errMsg(err)}`));
        return; // wait for the ack before processing further frames
      }

      if (frame.type !== MsgType.REQUEST) {
        send(socket, encodeError(frame.id, `unexpected frame type 0x${frame.type.toString(16)} after handshake`));
        continue;
      }
      if (!session) continue; // shutting down; drop
      handleRequest(socket, session, frame).catch((err) => {
        send(socket, encodeError(frame.id, errMsg(err)));
      });
    }
  });

  socket.on('close', () => {
    ctx.sockets.delete(socket);
    if (session) {
      ctx.sessions.delete(session.id);
      session.close().catch(() => {});
      ctx.log(`session ${session.id} closed`);
    }
  });
  socket.on('error', () => {
    /* swallow; 'close' fires next and tears the session down */
  });
}

/** Dispatch one REQUEST frame on the session and frame the reply (or ERROR). */
async function handleRequest(socket: net.Socket, session: Session, frame: Frame): Promise<void> {
  const req = parseRequest(frame); // throws → ERROR (caught by caller)
  const result = await session.dispatch(req);
  send(socket, encodeFrame(result.type, frame.id, result.payload));
}

/** Write if the socket is still alive. */
function send(socket: net.Socket, data: Buffer): void {
  if (!socket.destroyed && socket.writable) socket.write(data);
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
