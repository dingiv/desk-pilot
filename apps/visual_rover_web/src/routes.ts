/**
 * routes.ts — 分发层
 *
 * Koa middleware that maps HTTP requests → {@link AgentService} calls → HTTP responses.
 * All HTTP plumbing (body parsing, SSE handshake, status codes) lives here.
 * The service is called as a plain async function — zero knowledge of `@vrover/agent`.
 */
import http from 'node:http';
import type Koa from 'koa';
import type { TaskEvent } from '@vrover/agent';
import type { AgentService } from './service.js';

const MAX_BODY_BYTES = 1 << 16; // 64 KiB

export function createRoutes(service: AgentService, isDev: boolean): Koa.Middleware {
  return async (ctx, next) => {
    // ── health ──────────────────────────────────────────────────────────────
    if (ctx.method === 'GET' && ctx.path === '/api/health') {
      ctx.body = { ok: true, mode: isDev ? 'development' : 'production' };
      return;
    }

    // ── one-shot run ────────────────────────────────────────────────────────
    if (ctx.method === 'POST' && ctx.path === '/api/run') {
      let body: unknown;
      try {
        body = await readJsonBody(ctx.req);
      } catch (err) {
        ctx.status = 400;
        ctx.body = { error: `invalid request body: ${errMsg(err)}` };
        return;
      }
      const task =
        body && typeof body === 'object' && typeof (body as { task?: unknown }).task === 'string'
          ? (body as { task: string }).task.trim()
          : '';
      if (!task) {
        ctx.status = 400;
        ctx.body = { error: 'request body must be { task: string }' };
        return;
      }
      try {
        const rec = await service.execute(task);
        ctx.body = { result: rec.result, log: rec.log };
      } catch (err) {
        ctx.status = 500;
        ctx.body = { error: errMsg(err) };
      }
      return;
    }

    // ── streaming SSE ───────────────────────────────────────────────────────
    if (ctx.method === 'GET' && ctx.path === '/api/run/stream') {
      const task = (ctx.query.task as string | undefined)?.trim();
      if (!task) {
        ctx.status = 400;
        ctx.body = { error: 'query param `task` is required' };
        return;
      }
      const maxSteps = parseUintSafe(ctx.query.maxSteps as string | undefined);

      // SSE handshake
      ctx.req.socket.setTimeout(0);
      ctx.req.socket.setNoDelay(true);
      ctx.req.socket.setKeepAlive(true);
      ctx.set({
        'Content-Type': 'text/event-stream',
        'Cache-Control': 'no-cache',
        Connection: 'keep-alive',
        'X-Accel-Buffering': 'no',
      });
      ctx.respond = false;
      const res = ctx.res;
      res.writeHead(200);

      const sse = (data: unknown) => res.write(`data: ${JSON.stringify(data)}\n\n`);
      await service.stream(task, maxSteps, (ev: TaskEvent) => sse(ev));
      res.end();
      return;
    }

    // ── fall through to SPA middleware ───────────────────────────────────────
    await next();
  };
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

function readJsonBody(req: http.IncomingMessage): Promise<unknown> {
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
      try {
        resolve(data ? JSON.parse(data) : {});
      } catch (err) {
        reject(err);
      }
    });
  });
}

function parseUintSafe(raw: string | undefined): number | undefined {
  if (!raw) return undefined;
  const n = Number(raw);
  return Number.isInteger(n) && n > 0 ? n : undefined;
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
