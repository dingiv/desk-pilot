/** Shapes returned by the scout devtools HTTP/SSE API (mirrors `@vrover/scout-protocol`). */

export interface Bounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface UiElement {
  id: string;
  role: string;
  label: string;
  bounds: Bounds;
  attributes?: Record<string, unknown>;
}

export interface SessionInfo {
  id: string;
  backend: string;
}

export interface DevtoolsConfig {
  captureIntervalMs: number;
  activeSessionId?: string;
}

/** One frame pushed by the SSE `/api/sessions/:id/stream` endpoint. */
export interface Frame {
  type: 'frame';
  width: number;
  height: number;
  /** Base64-encoded PNG. */
  png: string;
  elements: UiElement[];
}
