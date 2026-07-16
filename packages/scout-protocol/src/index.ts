/**
 * @vrover/scout-protocol — the Visual Scout wire protocol.
 *
 * The contract between the Scout TCP server (`@vrover/scout`) and the standalone
 * client SDK (`@vrover/scout-client`): binary framing (`./protocol.ts`), the JSON
 * application messages (`./api.ts`), and the wire-level domain types
 * `UiElement`/`Bounds`/`CaptureResult` (`./types.ts`). A leaf package with no
 * internal dependencies — the client SDK depends on this and nothing else.
 */
// Types.
export type { Bounds, UiElement, CaptureResult } from './types.js';
// Framing + transport.
export {
  MAGIC,
  VERSION,
  HEADER_SIZE,
  MsgType,
  FrameDecoder,
  ProtocolError,
  encodeFrame,
  encodeJsonFrame,
  encodeCaptureBlob,
  decodeCaptureBlob,
} from './protocol.js';
export type { Frame } from './protocol.js';
// Application messages + codecs.
export {
  parseHandshakeRequest,
  parseHandshakeAck,
  parseRequest,
  encodeError,
} from './api.js';
export type {
  HandshakeRequest,
  HandshakeAck,
  Request,
  OkResult,
  ElementsResult,
  ErrorPayload,
} from './api.js';
