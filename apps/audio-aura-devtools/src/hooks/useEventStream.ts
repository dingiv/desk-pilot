import { useEffect, useRef } from 'react';
import type { StreamEvent } from '../types';
import { API_BASE } from '../apiBase';

/** Subscribe to the server's unified SSE event stream. */
export function useEventStream(onEvent: (ev: StreamEvent) => void): void {
  const cb = useRef(onEvent);
  cb.current = onEvent;
  useEffect(() => {
    const es = new EventSource(`${API_BASE}/api/stream`);
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        cb.current(JSON.parse(e.data) as StreamEvent);
      } catch {
        /* ignore keep-alive comments / malformed frames */
      }
    };
    return () => es.close();
  }, []);
}
