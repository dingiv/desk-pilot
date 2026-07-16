import http from 'node:http';
import type { IncomingMessage, Server, ServerResponse } from 'node:http';
import type { Platform } from '@vrover/platform';
import { decodeCaptureBlob, type HandshakeRequest, type Request } from '@vrover/scout-protocol';
import { Session } from './session.js';

/**
 * The Scout devtools service — a second, browser-friendly port on the Scout server
 * process. It exposes the same UI-operation + grounding capability as the custom TCP
 * protocol, but over **HTTP + SSE** so a browser can drive it (browsers cannot speak the
 * raw binary TCP protocol). It is in-process, so it does **not** re-implement anything:
 * every action routes through {@link Session.dispatch} — the identical path TCP clients
 * take — and screenshots are decoded from the same BLOB payload.
 *
 *   Browser (web UI) ⇄ HTTP/SSE ⇄ devtools service ⇄ Session.dispatch ⇄ Platform backend
 *
 * The service also lists/creates/closes sessions (including live TCP-client sessions, so
 * you can watch the brain drive a backend) and exposes a small runtime-config surface.
 * The custom Scout TCP protocol is untouched; this is an additive control plane.
 */

/** The Scout server state the devtools service drives (a subset of the TCP server's ctx). */
export interface DevtoolsContext {
  /** Shared session registry (TCP-client + devtools sessions alike). */
  sessions: Map<string, Session>;
  /** Mints a fresh backend terminal for a new session (same factory TCP handshakes use). */
  backendFactory: (req: HandshakeRequest) => Platform | Promise<Platform>;
  /** Shared id counter; devtools sessions take a `d_` prefix, TCP sessions `s_`. */
  seq: number;
  log: (message: string) => void;
}

export interface DevtoolsOptions {
  host?: string;
  /** `0` = OS-assigned (for tests). */
  port?: number;
  /** SSE capture tick rate, ms (default 1000; runtime-tunable via PUT /api/config). */
  captureIntervalMs?: number;
  log?: (message: string) => void;
}

export interface DevtoolsServer {
  readonly host: string;
  readonly port: number;
  close(): Promise<void>;
}

/** Runtime-tunable devtools/server state — the "configure server state" surface. */
export interface DevtoolsConfig {
  /** SSE capture tick rate, ms. */
  captureIntervalMs: number;
  /** Session the devtools UI is currently watching/driving, if any. */
  activeSessionId?: string;
}

const DEFAULT_PORT = 7881;
const DEFAULT_CAPTURE_INTERVAL_MS = 1000;
const MAX_BODY_BYTES = 1 << 16; // 64 KiB

/** Start the devtools HTTP/SSE service sharing the Scout server's session registry. */
export function startDevtoolsServer(
  ctx: DevtoolsContext,
  opts: DevtoolsOptions = {},
): Promise<DevtoolsServer> {
  const host = opts.host ?? '127.0.0.1';
  const port = opts.port ?? DEFAULT_PORT;
  const log = opts.log ?? ctx.log;
  const config: DevtoolsConfig = {
    captureIntervalMs: opts.captureIntervalMs ?? DEFAULT_CAPTURE_INTERVAL_MS,
  };

  const server: Server = http.createServer((req, res) => {
    void handle(req, res, ctx, config, log);
  });

  return new Promise<DevtoolsServer>((resolve, reject) => {
    server.on('error', reject);
    server.listen(port, host, () => {
      const addr = server.address();
      const actualPort = typeof addr === 'object' && addr ? addr.port : port;
      log(`Scout devtools service listening on http://${host}:${actualPort}`);
      resolve({
        host,
        port: actualPort,
        close: () => new Promise<void>((r) => server.close(() => r())),
      });
    });
  });
}

// ── routing ────────────────────────────────────────────────────────────────────

async function handle(
  req: IncomingMessage,
  res: ServerResponse,
  ctx: DevtoolsContext,
  config: DevtoolsConfig,
  log: (m: string) => void,
): Promise<void> {
  setCORS(res);
  const url = new URL(req.url ?? '/', 'http://localhost');
  if (req.method === 'OPTIONS') {
    res.writeHead(204);
    res.end();
    return;
  }

  if (url.pathname === '/api/health' && req.method === 'GET') {
    return sendJson(res, 200, { ok: true, sessions: ctx.sessions.size });
  }

  if (url.pathname === '/api/config') {
    if (req.method === 'GET') return sendJson(res, 200, config);
    if (req.method === 'PUT') {
      const body = await readJson(req);
      if (typeof body.captureIntervalMs === 'number' && body.captureIntervalMs >= 0) {
        config.captureIntervalMs = body.captureIntervalMs;
      }
      if (typeof body.activeSessionId === 'string') config.activeSessionId = body.activeSessionId;
      else if (body.activeSessionId === null) config.activeSessionId = undefined;
      return sendJson(res, 200, config);
    }
  }

  if (url.pathname === '/api/sessions') {
    if (req.method === 'GET') {
      const sessions = [...ctx.sessions.values()].map((s) => ({ id: s.id, backend: backendNameOf(s) }));
      return sendJson(res, 200, { sessions });
    }
    if (req.method === 'POST') {
      const body = await readJson(req);
      const hint: HandshakeRequest = { client: 'devtools' };
      if (typeof body.backend === 'string') hint.backend = body.backend;
      const backend = await ctx.backendFactory(hint);
      const id = `d_${(++ctx.seq).toString(36)}`;
      const session = new Session(id, backend);
      ctx.sessions.set(id, session);
      log(`devtools session ${id} created (backend: ${backendNameOf(session)})`);
      return sendJson(res, 201, { id, backend: backendNameOf(session) });
    }
  }

  const match = url.pathname.match(/^\/api\/sessions\/([^/]+)(\/[^/]*)?$/);
  if (match) {
    const id = match[1]!;
    const sub = match[2] ?? '';
    const session = ctx.sessions.get(id);
    if (!session) return sendJson(res, 404, { error: `session ${id} not found` });

    if (sub === '' && req.method === 'DELETE') {
      ctx.sessions.delete(id);
      await session.close().catch(() => {});
      log(`devtools session ${id} closed`);
      return sendJson(res, 200, { ok: true });
    }
    if (sub === '/capture' && req.method === 'GET') {
      const { payload } = await session.dispatch({ method: 'capture' });
      const { png } = decodeCaptureBlob(payload);
      return sendBytes(res, 200, 'image/png', png);
    }
    if (sub === '/elements' && req.method === 'GET') {
      const { payload } = await session.dispatch({ method: 'elements' });
      return sendBuffer(res, 200, 'application/json; charset=utf-8', payload);
    }
    if (sub === '/stream' && req.method === 'GET') {
      return startSse(req, res, session, config);
    }
    if (req.method === 'POST') {
      const body = await readJson(req);
      let request: Request;
      try {
        request = buildActionRequest(sub.slice(1), body);
      } catch (err) {
        return sendJson(res, 400, { error: errMsg(err) });
      }
      await session.dispatch(request);
      return sendJson(res, 200, { ok: true });
    }
  }

  if (url.pathname === '/' && req.method === 'GET') {
    return sendText(res, 200, 'text/html; charset=utf-8', LANDING_HTML);
  }
  return sendJson(res, 404, { error: 'not found' });
}

/** Translate an HTTP action body into the `Request` shape Session.dispatch expects. */
function buildActionRequest(method: string, body: Record<string, unknown>): Request {
  switch (method) {
    case 'click':
      return { method: 'click', x: num(body.x), y: num(body.y) };
    case 'type':
      return { method: 'type', text: String(body.text ?? '') };
    case 'scroll':
      return {
        method: 'scroll',
        x: num(body.x),
        y: num(body.y),
        direction: body.direction === 'up' ? 'up' : 'down',
      };
    case 'keypress':
      return { method: 'keypress', keys: String(body.keys ?? '') };
    default:
      throw new Error(`unknown action "${method}"`);
  }
}

/** SSE: push capture + elements for a session on connect and on every config tick. */
function startSse(req: IncomingMessage, res: ServerResponse, session: Session, config: DevtoolsConfig): void {
  res.writeHead(200, {
    'Content-Type': 'text/event-stream',
    'Cache-Control': 'no-cache',
    Connection: 'keep-alive',
    ...CORS_HEADERS,
  });
  let timer: NodeJS.Timeout | undefined;
  const push = async (): Promise<void> => {
    try {
      const cap = await session.dispatch({ method: 'capture' });
      const { width, height, png } = decodeCaptureBlob(cap.payload);
      const els = await session.dispatch({ method: 'elements' });
      const { elements } = JSON.parse(els.payload.toString('utf8')) as { elements: unknown[] };
      const frame = { type: 'frame', width, height, png: png.toString('base64'), elements };
      res.write(`data: ${JSON.stringify(frame)}\n\n`);
    } catch (err) {
      res.write(`event: error\ndata: ${JSON.stringify({ error: errMsg(err) })}\n\n`);
    }
  };
  void push();
  const tick = (): void => {
    void push();
    timer = setTimeout(tick, Math.max(50, config.captureIntervalMs));
  };
  timer = setTimeout(tick, Math.max(50, config.captureIntervalMs));
  req.on('close', () => {
    if (timer) clearTimeout(timer);
  });
}

// ── helpers ────────────────────────────────────────────────────────────────────

const CORS_HEADERS: Readonly<Record<string, string>> = {
  'Access-Control-Allow-Origin': '*',
  'Access-Control-Allow-Methods': 'GET, POST, PUT, DELETE, OPTIONS',
  'Access-Control-Allow-Headers': 'content-type',
};

function setCORS(res: ServerResponse): void {
  for (const [k, v] of Object.entries(CORS_HEADERS)) res.setHeader(k, v);
}

function backendNameOf(session: Session): string {
  return session.backend.constructor.name;
}

function num(v: unknown): number {
  if (typeof v !== 'number' || !Number.isFinite(v)) throw new Error('expected a number');
  return v;
}

async function readJson(req: IncomingMessage): Promise<Record<string, unknown>> {
  const data = await readBody(req);
  if (!data) return {};
  try {
    const parsed = JSON.parse(data);
    return typeof parsed === 'object' && parsed !== null ? (parsed as Record<string, unknown>) : {};
  } catch {
    throw new Error('request body is not valid JSON');
  }
}

function readBody(req: IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    let data = '';
    let tooLarge = false;
    req.setEncoding('utf8');
    req.on('data', (chunk: string) => {
      data += chunk;
      if (Buffer.byteLength(data) > MAX_BODY_BYTES) {
        tooLarge = true;
        req.destroy();
      }
    });
    req.on('error', reject);
    req.on('end', () => {
      if (tooLarge) return reject(new Error(`body exceeds ${MAX_BODY_BYTES} bytes`));
      resolve(data);
    });
  });
}

function sendJson(res: ServerResponse, status: number, payload: unknown): void {
  sendBuffer(res, status, 'application/json; charset=utf-8', Buffer.from(JSON.stringify(payload)));
}

function sendBuffer(res: ServerResponse, status: number, contentType: string, body: Buffer): void {
  res.writeHead(status, { 'Content-Type': contentType, 'Content-Length': body.length });
  res.end(body);
}

function sendBytes(res: ServerResponse, status: number, contentType: string, body: Buffer): void {
  sendBuffer(res, status, contentType, body);
}

function sendText(res: ServerResponse, status: number, contentType: string, body: string): void {
  res.writeHead(status, { 'Content-Type': contentType, 'Content-Length': Buffer.byteLength(body) });
  res.end(body);
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

const LANDING_HTML = `<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Scout DevTools</title>
<style>body{font:14px/1.5 system-ui,sans-serif;max-width:640px;margin:40px auto;padding:0 16px;color:#1f2328}</style>
</head><body>
<h1>Scout DevTools service</h1>
<p>This is the in-process HTTP/SSE devtools API. Open the <strong>devtools web UI</strong> app
(<code>apps/omni_scout_devtools</code>) to use it, or call the REST/SSE endpoints under <code>/api/</code> directly.</p>
<p><code>GET /api/health</code> · <code>GET /api/sessions</code> · <code>POST /api/sessions</code>
· <code>GET /api/sessions/:id/capture</code> · <code>GET /api/sessions/:id/stream</code> (SSE)</p>
</body></html>`;
