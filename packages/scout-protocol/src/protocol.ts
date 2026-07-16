import type { CaptureResult } from './types.js';

/**
 * Visual Scout wire protocol — the binary framing shared by the TCP server
 * (`@vrover/scout`) and the standalone client SDK (`@vrover/scout-client`).
 *
 * This is the single source of truth for the *transport*. Application message
 * *shapes* — the request `method` payloads, handshake config, result objects —
 * live in `./api.ts`; this file only knows how to move bytes.
 *
 * Frame layout (big-endian, 12-byte header + payload):
 *
 *   offset 0   2   magic     0x53 0x43  ('SC')
 *   offset 2   1   version   1
 *   offset 3   1   type      {@link MsgType}
 *   offset 4   4   id        u32 — correlates request ↔ response; 0 for handshake
 *   offset 8   4   length    u32 — payload byte length (excludes the header)
 *
 * Control messages (handshake / request / result / error) carry a UTF-8 JSON
 * payload. Capture results ride as a raw {@link MsgType.BLOB} whose payload is
 * `[u32 width][u32 height][png bytes]` — no base64.
 */

/** Frame magic bytes: ASCII `'SC'` (`0x53 0x43`). A tuple so indexing yields clean numbers. */
export const MAGIC = [0x53, 0x43] as const;
/** Current protocol version a frame must carry (and peers must agree on). */
export const VERSION = 1;
/** Fixed size of the frame header, in bytes. */
export const HEADER_SIZE = 12;

/** Message type carried in frame header byte 3. */
export const MsgType = {
  /** Client → server: request a session (JSON {@link HandshakeRequest}). id = 0. */
  HAND_SHAKE: 0x01,
  /** Server → client: session created (JSON {@link HandshakeAck}). id = 0. */
  HAND_SHAKE_ACK: 0x02,
  /** Server → client: in-band error (JSON `{ error }`). id = failing request, or 0. */
  ERROR: 0x03,
  /** Client → server: an application request (JSON {@link Request}). id = request id. */
  REQUEST: 0x10,
  /** Server → client: a JSON result. id matches the request. */
  RESULT: 0x11,
  /** Server → client: a binary result (e.g. a screenshot). id matches the request. */
  BLOB: 0x12,
} as const;
export type MsgType = (typeof MsgType)[keyof typeof MsgType];

/** A fully-decoded frame: type + correlation id + raw payload bytes. */
export interface Frame {
  type: MsgType;
  id: number;
  payload: Buffer;
}

/** Thrown when bytes on the wire don't conform to the framing above. */
export class ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ProtocolError';
  }
}

/**
 * Encode one frame. `payload` is either a raw `Buffer` (for {@link MsgType.BLOB})
 * or a string (typically `JSON.stringify(...)` for control messages).
 */
export function encodeFrame(type: MsgType, id: number, payload: Buffer | string): Buffer {
  const body = typeof payload === 'string' ? Buffer.from(payload, 'utf8') : payload;
  const header = Buffer.allocUnsafe(HEADER_SIZE);
  header[0] = MAGIC[0];
  header[1] = MAGIC[1];
  header[2] = VERSION;
  header[3] = type;
  header.writeUInt32BE(id >>> 0, 4);
  header.writeUInt32BE(body.length, 8);
  return Buffer.concat([header, body]);
}

/** Shorthand: `JSON.stringify` the value and frame it. */
export function encodeJsonFrame(type: MsgType, id: number, value: unknown): Buffer {
  return encodeFrame(type, id, JSON.stringify(value));
}

/**
 * Incremental frame reassembler. Feed it raw socket chunks via {@link push};
 * it returns any frames that became complete. Frames split across TCP segment
 * boundaries are handled: it buffers a partial header, then a partial payload,
 * until each frame is whole.
 *
 * @throws {ProtocolError} if the buffered bytes start with a bad magic / version.
 */
export class FrameDecoder {
  private buffer: Buffer = Buffer.alloc(0);

  /** Feed a chunk; returns zero or more fully-assembled frames. */
  push(chunk: Buffer): Frame[] {
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
    const frames: Frame[] = [];
    for (;;) {
      if (this.buffer.length < HEADER_SIZE) break;
      // `readUInt8`/`readUInt32BE` (not indexing) keep types clean under noUncheckedIndexedAccess.
      const m0 = this.buffer.readUInt8(0);
      const m1 = this.buffer.readUInt8(1);
      if (m0 !== MAGIC[0] || m1 !== MAGIC[1]) {
        throw new ProtocolError(
          `bad frame magic: 0x${m0.toString(16).padStart(2, '0')}${m1.toString(16).padStart(2, '0')}`,
        );
      }
      const version = this.buffer.readUInt8(2);
      if (version !== VERSION) {
        throw new ProtocolError(`unsupported protocol version ${version}`);
      }
      const length = this.buffer.readUInt32BE(8);
      if (this.buffer.length < HEADER_SIZE + length) break; // payload not fully received yet
      const type = this.buffer.readUInt8(3) as MsgType;
      const id = this.buffer.readUInt32BE(4);
      const payload = Buffer.from(this.buffer.subarray(HEADER_SIZE, HEADER_SIZE + length));
      frames.push({ type, id, payload });
      this.buffer = this.buffer.subarray(HEADER_SIZE + length);
    }
    return frames;
  }
}

// ── capture BLOB payload ─────────────────────────────────────────────────────
// BLOB payload for a screenshot: [u32 width][u32 height][raw PNG bytes].

/** Build the binary payload for a screenshot BLOB frame. */
export function encodeCaptureBlob(width: number, height: number, png: Buffer): Buffer {
  const out = Buffer.allocUnsafe(8 + png.length);
  out.writeUInt32BE(width >>> 0, 0);
  out.writeUInt32BE(height >>> 0, 4);
  png.copy(out, 8);
  return out;
}

/** Decode the binary payload of a screenshot BLOB frame. */
export function decodeCaptureBlob(payload: Buffer): CaptureResult {
  if (payload.length < 8) throw new ProtocolError('capture blob payload too short');
  return {
    width: payload.readUInt32BE(0),
    height: payload.readUInt32BE(4),
    png: Buffer.from(payload.subarray(8)),
  };
}
