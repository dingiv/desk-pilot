import { useEffect, useRef } from 'react';
import { API_BASE } from '../apiBase';
import type { AsrSegment } from '../types';

/**
 * DATA plane — subscribe to the daemon's recognition segments (`GET /api/asr_stream`, native
 * EventSource, auto-reconnect). Each `data:` frame is one `AsrSegment` (interim /
 * calibrated_interim / final / correction); `onSegment` updates the caller's local utterance list.
 * Low-latency, every event (NOT throttled like the control plane).
 */
export function useAuraSegments(onSegment: (seg: AsrSegment) => void): void {
  const cb = useRef(onSegment);
  cb.current = onSegment;
  useEffect(() => {
    const es = new EventSource(`${API_BASE}/api/asr_stream`);
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        cb.current(JSON.parse(e.data) as AsrSegment);
      } catch {
        /* ignore keep-alive comments / malformed frames */
      }
    };
    return () => es.close();
  }, []);
}
