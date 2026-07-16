/**
 * Visual Scout application message shapes — the JSON payloads carried inside the
 * binary frames defined in `./protocol.ts`. This is the contract the server
 * (`@vrover/scout`) and the client SDK (`@vrover/scout-client`) agree on at the
 * *application* layer; the raw framing lives in `./protocol.ts`.
 *
 * One source of truth for message shapes. `UiElement` (from `./types.ts`) is
 * already JSON-serializable, so it crosses the wire as-is.
 */

import type { UiElement } from './types.js';
import { ProtocolError, encodeJsonFrame, MsgType, type Frame } from './protocol.js';

// ── handshake ────────────────────────────────────────────────────────────────

/**
 * Client requirements, sent as the JSON payload of the first {@link MsgType.HAND_SHAKE}
 * frame. The server uses it to mint the right backend for the session. All fields
 * are optional/hints today; the contract can grow without breaking older clients.
 */
export interface HandshakeRequest {
  /** Free-form client id (e.g. name + version), informational. */
  client?: string;
  /** Requested backend hint, e.g. `'multi-screen'` | `'desktop'`. Server may ignore. */
  backend?: string;
}

/** Server's reply once a session is created — the {@link MsgType.HAND_SHAKE_ACK} payload. */
export interface HandshakeAck {
  sessionId: string;
  /** Protocol version the server speaks (matches {@link VERSION}). */
  version: number;
  /** Name of the backend driving this session, e.g. `'multi-screen'`. */
  backend: string;
}

// ── requests ─────────────────────────────────────────────────────────────────

/** A client→server application request — one `method` per `Platform` primitive. */
export type Request =
  | { method: 'capture' }
  | { method: 'elements' }
  | { method: 'click'; x: number; y: number }
  | { method: 'type'; text: string }
  | { method: 'scroll'; x: number; y: number; direction: 'up' | 'down' }
  | { method: 'keypress'; keys: string };

// ── results ──────────────────────────────────────────────────────────────────

/** `RESULT` payload for the action endpoints (click / type / scroll / keypress). */
export interface OkResult {
  ok: true;
}

/** `RESULT` payload for an `elements` request. */
export interface ElementsResult {
  elements: UiElement[];
}

/** `ERROR` payload, correlated to the failing request by frame id (0 if handshake). */
export interface ErrorPayload {
  error: string;
}

// ── frame ↔ message codecs ───────────────────────────────────────────────────

/** Parse and validate a JSON frame payload, throwing on malformed JSON/shape. */
function parseJsonPayload<T>(frame: Frame, what: string): T {
  try {
    return JSON.parse(frame.payload.toString('utf8')) as T;
  } catch {
    throw new ProtocolError(`${what} payload is not valid JSON`);
  }
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null;
}

/** Decode + validate a {@link HandshakeRequest} from a `HAND_SHAKE` frame. */
export function parseHandshakeRequest(frame: Frame): HandshakeRequest {
  const body = parseJsonPayload<unknown>(frame, 'handshake');
  if (body === null || isObject(body)) {
    const req: HandshakeRequest = {};
    if (isObject(body)) {
      if (body.client !== undefined && typeof body.client !== 'string') {
        throw new ProtocolError('handshake.client must be a string');
      }
      if (body.backend !== undefined && typeof body.backend !== 'string') {
        throw new ProtocolError('handshake.backend must be a string');
      }
      req.client = body.client as string | undefined;
      req.backend = body.backend as string | undefined;
    }
    return req;
  }
  throw new ProtocolError('handshake payload must be a JSON object');
}

/** Decode a {@link HandshakeAck} from a `HAND_SHAKE_ACK` frame. */
export function parseHandshakeAck(frame: Frame): HandshakeAck {
  const body = parseJsonPayload<HandshakeAck>(frame, 'handshake-ack');
  if (!isObject(body) || typeof body.sessionId !== 'string' || typeof body.backend !== 'string') {
    throw new ProtocolError('malformed handshake-ack');
  }
  return body;
}

/**
 * Decode + validate a {@link Request} from a `REQUEST` frame. Throws a
 * {@link ProtocolError} (→ in-band `ERROR`) on an unknown method or bad args —
 * the protocol's replacement for the old HTTP 400.
 */
export function parseRequest(frame: Frame): Request {
  const body = parseJsonPayload<unknown>(frame, 'request');
  if (!isObject(body) || typeof body.method !== 'string') {
    throw new ProtocolError('request requires { method: string }');
  }
  switch (body.method) {
    case 'capture':
    case 'elements':
      return { method: body.method };
    case 'click': {
      const { x, y } = body;
      if (typeof x !== 'number' || typeof y !== 'number') {
        throw new ProtocolError('click requires numeric { x, y }');
      }
      return { method: 'click', x, y };
    }
    case 'type': {
      if (typeof body.text !== 'string') throw new ProtocolError('type requires { text: string }');
      return { method: 'type', text: body.text };
    }
    case 'scroll': {
      const { x, y, direction } = body;
      if (typeof x !== 'number' || typeof y !== 'number') {
        throw new ProtocolError('scroll requires numeric { x, y }');
      }
      if (direction !== 'up' && direction !== 'down') {
        throw new ProtocolError('scroll direction must be "up" or "down"');
      }
      return { method: 'scroll', x, y, direction };
    }
    case 'keypress': {
      if (typeof body.keys !== 'string') throw new ProtocolError('keypress requires { keys: string }');
      return { method: 'keypress', keys: body.keys };
    }
    default:
      throw new ProtocolError(`unknown request method: ${body.method}`);
  }
}

/** Encode an `ERROR` frame (`{ error }`) correlated to `id`. */
export function encodeError(id: number, error: string): Buffer {
  return encodeJsonFrame(MsgType.ERROR, id, { error });
}
