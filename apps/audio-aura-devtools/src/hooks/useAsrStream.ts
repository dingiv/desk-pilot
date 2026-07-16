import { useEffect, useRef } from 'react';
import type { AsrEvent } from '../types';
import { API_BASE } from '../apiBase';

/**
 * Subscribe to the aura-daemon's live Stage1 SSE stream (`GET /api/stream`). Each parsed
 * `AsrEvent` is handed to `onEvent`. Auto-reconnect is handled by the browser's EventSource.
 */
export function useAsrStream(onEvent: (ev: AsrEvent) => void): void {
  const cb = useRef(onEvent);
  cb.current = onEvent;
  useEffect(() => {
    const es = new EventSource(`${API_BASE}/api/stream`);
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        cb.current(JSON.parse(e.data) as AsrEvent);
      } catch {
        /* ignore keep-alive comments / malformed frames */
      }
    };
    return () => es.close();
  }, []);
}
