/**
 * Typed client for the scout **devtools service** (HTTP/SSE). All URLs are relative
 * (`/api/...`) and reach the scout devtools port through the Vite proxy (dev/preview), so
 * the browser stays single-origin with the API — no CORS, SSE streams cleanly. The service
 * reuses `Session.dispatch` server-side, so these map 1:1 onto the TCP protocol's methods.
 */
import type { DevtoolsConfig, Frame, SessionInfo, UiElement } from './types';

async function getJson(path: string): Promise<unknown> {
  const r = await fetch(path);
  const text = await r.text();
  const json = text ? (JSON.parse(text) as unknown) : {};
  if (!r.ok) {
    const err = (json as { error?: string }).error ?? `HTTP ${r.status}`;
    throw new Error(err);
  }
  return json;
}

async function reqJson(path: string, method: string, body?: unknown): Promise<unknown> {
  const r = await fetch(path, {
    method,
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  });
  const text = await r.text();
  const json = text ? (JSON.parse(text) as unknown) : {};
  if (!r.ok) {
    const err = (json as { error?: string }).error ?? `HTTP ${r.status}`;
    throw new Error(err);
  }
  return json;
}

export async function getHealth(): Promise<{ ok: boolean; sessions: number }> {
  return (await getJson('/api/health')) as { ok: boolean; sessions: number };
}

export async function listSessions(): Promise<SessionInfo[]> {
  const data = (await getJson('/api/sessions')) as { sessions?: SessionInfo[] };
  return data.sessions ?? [];
}

export async function createSession(backend?: string): Promise<SessionInfo> {
  return (await reqJson('/api/sessions', 'POST', backend ? { backend } : {})) as SessionInfo;
}

export async function deleteSession(id: string): Promise<void> {
  await reqJson(`/api/sessions/${id}`, 'DELETE');
}

/** Capture URL — fetched separately so the PNG bytes go straight to `createImageBitmap`. */
export function captureUrl(id: string): string {
  return `/api/sessions/${id}/capture`;
}

export async function capture(id: string): Promise<Blob> {
  const r = await fetch(captureUrl(id));
  if (!r.ok) throw new Error(`capture HTTP ${r.status}`);
  return r.blob();
}

export async function getElements(id: string): Promise<UiElement[]> {
  const data = (await getJson(`/api/sessions/${id}/elements`)) as { elements?: UiElement[] };
  return data.elements ?? [];
}

export async function action(
  id: string,
  method: 'click' | 'type' | 'scroll' | 'keypress',
  body: Record<string, unknown>,
): Promise<void> {
  await reqJson(`/api/sessions/${id}/${method}`, 'POST', body);
}

export async function getConfig(): Promise<DevtoolsConfig> {
  return (await getJson('/api/config')) as DevtoolsConfig;
}

export async function putConfig(patch: Partial<DevtoolsConfig>): Promise<DevtoolsConfig> {
  return (await reqJson('/api/config', 'PUT', patch)) as DevtoolsConfig;
}

/**
 * Open an SSE live stream for a session. Returns a closer. `onFrame` is async to allow
 * decoding the base64 PNG into an ImageBitmap off the call site.
 */
export function streamSession(
  id: string,
  onFrame: (frame: Frame) => void,
  onError?: (e: Event) => void,
): () => void {
  const es = new EventSource(`/api/sessions/${id}/stream`);
  es.onmessage = (ev: MessageEvent<string>) => {
    try {
      onFrame(JSON.parse(ev.data) as Frame);
    } catch {
      /* ignore malformed frames */
    }
  };
  es.onerror = (e) => onError?.(e);
  return () => es.close();
}
