import type { GroundingSource, Platform } from '@vrover/platform';
import { PlatformGroundingSource } from './grounding.js';
import { Walker } from './walker.js';
import { MsgType, encodeCaptureBlob, type Request } from '@vrover/scout-protocol';

/**
 * One session of the Visual Scout server — the **session-level state** for a
 * single client connection.
 *
 * A session abstracts *one operation terminal attached to a platform*: it owns a
 * {@link Platform} (screen capture + keyboard/mouse), a {@link GroundingSource}
 * built on it, and a {@link Walker} (graph-walker state — a placeholder today).
 * Its lifetime is the connection's: the server creates it on a successful
 * handshake and disposes it when the socket closes.
 *
 * Per `docs/decisions.md` D10: server-level knowledge (the shared graph map)
 * lives in the server; the walker is per-session state and lives here. Keeping
 * them apart is what lets multiple clients share one map but navigate
 * independently.
 */
export class Session {
  readonly id: string;
  readonly backend: Platform;
  readonly grounding: GroundingSource;
  /** Graph-walker state for this connection (placeholder — see {@link Walker}). */
  readonly walker = new Walker();

  constructor(id: string, backend: Platform, grounding?: GroundingSource) {
    this.id = id;
    this.backend = backend;
    this.grounding = grounding ?? new PlatformGroundingSource(backend);
  }

  /**
   * Run one application {@link Request} against the terminal and produce the raw
   * frame payload to send back. Returns `{ type, payload }`; the caller stamps
   * the correlation id and encodes the frame.
   *
   * - `capture` → a {@link MsgType.BLOB} (`[u32 width][u32 height][png]`).
   * - `elements` → a {@link MsgType.RESULT} JSON `{ elements }`.
   * - click / type / scroll / keypress → a {@link MsgType.RESULT} JSON `{ ok: true }`.
   *
   * Throws on backend failure; the caller turns that into an in-band `ERROR`.
   */
  async dispatch(req: Request): Promise<{ type: MsgType; payload: Buffer }> {
    switch (req.method) {
      case 'capture': {
        const s = await this.backend.captureScreen();
        return { type: MsgType.BLOB, payload: encodeCaptureBlob(s.width, s.height, s.png) };
      }
      case 'elements': {
        const elements = await this.grounding.detect();
        return { type: MsgType.RESULT, payload: Buffer.from(JSON.stringify({ elements })) };
      }
      case 'click':
        await this.backend.performClick(req.x, req.y);
        return { type: MsgType.RESULT, payload: OK };
      case 'type':
        await this.backend.performType(req.text);
        return { type: MsgType.RESULT, payload: OK };
      case 'scroll':
        await this.backend.performScroll(req.x, req.y, req.direction);
        return { type: MsgType.RESULT, payload: OK };
      case 'keypress':
        await this.backend.performKeypress(req.keys);
        return { type: MsgType.RESULT, payload: OK };
    }
  }

  /** Best-effort teardown: dispose the backend if it exposes `close()`. */
  async close(): Promise<void> {
    const closeable = this.backend as unknown as { close?: () => unknown };
    if (typeof closeable.close === 'function') {
      await closeable.close();
    }
  }
}

/** Pre-encoded `{ ok: true }` RESULT payload — every action returns the same bytes. */
const OK = Buffer.from(JSON.stringify({ ok: true }));
