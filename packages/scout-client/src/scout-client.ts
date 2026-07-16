import net from 'node:net';
import {
  FrameDecoder,
  MsgType,
  decodeCaptureBlob,
  encodeJsonFrame,
  parseHandshakeAck,
  type CaptureResult,
  type ElementsResult,
  type ErrorPayload,
  type Frame,
  type HandshakeAck,
  type HandshakeRequest,
  type Request,
  type UiElement,
} from '@vrover/scout-protocol';

/**
 * Programmer-facing client for the Visual Scout server — the "人类 JS 脚本 API"
 * from `docs/design.md`: a thin JS interface over the custom TCP protocol, for
 * writing automation scripts against whatever backend a Scout server is driving.
 *
 * This is a **standalone package for third-party developers**: it depends *only*
 * on `@vrover/scout-protocol` (the wire contract) — never on the rest of the
 * project. The brain-side `RemotePlatform` (`@vrover/agent`) is the sole internal
 * consumer of this SDK.
 *
 * One long-lived connection per instance. The constructor opens the socket and
 * starts the handshake; {@link ready} resolves with the server's ack (assigning
 * the session id), or use the ergonomic {@link ScoutClient.connect} factory which
 * awaits it for you. Every method awaits {@link ready} first, sends a
 * {@link MsgType.REQUEST}, and resolves on the matching reply — or rejects on
 * {@link MsgType.ERROR}. Screenshots arrive as a raw binary BLOB (no base64).
 *
 * @example
 *   const client = await ScoutClient.connect('127.0.0.1', 7878);
 *   const els = await client.elements();
 *   await client.click(640, 340);
 *   await client.type('admin');
 *   const shot = await client.capture();
 *   await client.close();
 */
export class ScoutClient {
  readonly host: string;
  readonly port: number;
  /** Resolves with the handshake ack once the session is established. */
  readonly ready: Promise<HandshakeAck>;

  private readonly socket: net.Socket;
  private readonly decoder = new FrameDecoder();
  private readonly pending = new Map<number, { resolve: (f: Frame) => void; reject: (e: Error) => void }>();
  private seq = 0;
  private torn = false;
  private ack: HandshakeAck | undefined;
  private resolveReady!: (ack: HandshakeAck) => void;
  private rejectReady!: (err: Error) => void;

  constructor(host: string, port: number, req: HandshakeRequest = {}) {
    this.host = host;
    this.port = port;
    this.ready = new Promise<HandshakeAck>((resolve, reject) => {
      this.resolveReady = resolve;
      this.rejectReady = reject;
    });

    this.socket = net.createConnection({ host, port });
    this.socket.on('connect', () => this.socket.write(encodeJsonFrame(MsgType.HAND_SHAKE, 0, req)));
    this.socket.on('data', (chunk: Buffer) => this.onData(chunk));
    this.socket.on('error', (err) => this.teardown(err));
    this.socket.on('close', () => this.teardown(new Error('connection closed')));
  }

  /** Async factory: connect, await the handshake, return a ready-to-use client. */
  static async connect(host: string, port: number, req: HandshakeRequest = {}): Promise<ScoutClient> {
    const client = new ScoutClient(host, port, req);
    await client.ready;
    return client;
  }

  /** Session id assigned by the server (available once {@link ready} has resolved). */
  get sessionId(): string {
    if (!this.ack) throw new Error('sessionId is unavailable before the handshake completes (await .ready).');
    return this.ack.sessionId;
  }

  /** Backend name reported by the server (available once {@link ready} has resolved). */
  get backend(): string {
    if (!this.ack) throw new Error('backend is unavailable before the handshake completes (await .ready).');
    return this.ack.backend;
  }

  /** Convenience: the ack info as a small health object. */
  async health(): Promise<{ ok: true; backend: string; sessionId: string }> {
    const ack = await this.ready;
    return { ok: true, backend: ack.backend, sessionId: ack.sessionId };
  }

  /** Capture the current screen as a PNG (decoded from the BLOB reply). */
  async capture(): Promise<CaptureResult> {
    const frame = await this.call({ method: 'capture' });
    if (frame.type !== MsgType.BLOB) {
      throw new Error(`capture: expected BLOB reply, got 0x${frame.type.toString(16)}`);
    }
    return decodeCaptureBlob(frame.payload);
  }

  /** UI elements available for interaction right now. */
  async elements(): Promise<UiElement[]> {
    const frame = await this.call({ method: 'elements' });
    return (JSON.parse(frame.payload.toString('utf8')) as ElementsResult).elements;
  }

  /** Click at screen coordinates. */
  async click(x: number, y: number): Promise<void> {
    await this.call({ method: 'click', x, y });
  }

  /** Type text into whatever currently has focus. */
  async type(text: string): Promise<void> {
    await this.call({ method: 'type', text });
  }

  /** Scroll at the given coordinates. */
  async scroll(x: number, y: number, direction: 'up' | 'down'): Promise<void> {
    await this.call({ method: 'scroll', x, y, direction });
  }

  /** Press a key combo, e.g. "Return", "ctrl+s". */
  async keypress(keys: string): Promise<void> {
    await this.call({ method: 'keypress', keys });
  }

  /** Close the connection. Pending calls reject with a 'client closed' error. */
  close(): void {
    this.teardown(new Error('client closed'));
    this.socket.destroy();
  }

  // ── internals ──────────────────────────────────────────────────────────────

  /** Send a request and await its matching reply frame. */
  private async call(req: Request): Promise<Frame> {
    await this.ready;
    return new Promise<Frame>((resolve, reject) => {
      const id = this.nextId();
      this.pending.set(id, { resolve, reject }); // set before write so teardown sees it
      this.socket.write(encodeJsonFrame(MsgType.REQUEST, id, req));
    });
  }

  private nextId(): number {
    this.seq = (this.seq + 1) >>> 0;
    if (this.seq === 0) this.seq = 1; // 0 is reserved for handshake
    return this.seq;
  }

  private onData(chunk: Buffer): void {
    let frames: Frame[];
    try {
      frames = this.decoder.push(chunk);
    } catch (err) {
      this.teardown(err instanceof Error ? err : new Error(String(err)));
      return;
    }
    for (const frame of frames) this.onFrame(frame);
  }

  private onFrame(frame: Frame): void {
    switch (frame.type) {
      case MsgType.HAND_SHAKE_ACK: {
        try {
          this.ack = parseHandshakeAck(frame);
          this.resolveReady(this.ack);
        } catch (err) {
          this.teardown(err instanceof Error ? err : new Error(String(err)));
        }
        return;
      }
      case MsgType.ERROR: {
        const error = (safeJson(frame.payload) as ErrorPayload | null)?.error ?? 'unknown error';
        if (frame.id === 0) this.rejectReady(new Error(error));
        else this.settle(frame.id, null, new Error(error));
        return;
      }
      case MsgType.RESULT:
      case MsgType.BLOB: {
        this.settle(frame.id, frame, null);
        return;
      }
      default:
        return; // unexpected frame type — ignore
    }
  }

  /** Resolve/reject a pending request by id, then drop it. */
  private settle(id: number, frame: Frame | null, err: Error | null): void {
    const p = this.pending.get(id);
    if (!p) return;
    this.pending.delete(id);
    if (err) p.reject(err);
    else p.resolve(frame!);
  }

  /** Idempotent teardown: fail the handshake + all pending calls. */
  private teardown(err: Error): void {
    if (this.torn) return;
    this.torn = true;
    this.rejectReady(err);
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();
  }
}

function safeJson(payload: Buffer): unknown {
  try {
    return JSON.parse(payload.toString('utf8'));
  } catch {
    return null;
  }
}
