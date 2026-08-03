import { useEffect, useRef } from 'react';
import { API_BASE } from '../apiBase';

/**
 * Subscribe to aura-daemon's state-change pings: `GET /api/stream?state_changed_frequency=<ms>`
 * (native EventSource — browser handles auto-reconnect). On each `{type:"state_changed"}` ping,
 * call `onPing`, which GETs /api/state and re-renders the snapshot.
 *
 * The server throttles pings to ≥ `freqMs` (floor 250ms = max 4Hz); the caller renders at its
 * own pace and may freely skip pings. `onPing` is held in a ref so the EventSource isn't torn
 * down on every render.
 */
export function useAuraStream(onPing: () => void, freqMs = 400): void {
  const cb = useRef(onPing);
  cb.current = onPing;
  useEffect(() => {
    const es = new EventSource(`${API_BASE}/api/stream?state_changed_frequency=${freqMs}`);
    es.onmessage = (e: MessageEvent<string>) => {
      try {
        const ev = JSON.parse(e.data) as { type?: string };
        if (ev?.type === 'state_changed') cb.current();
      } catch {
        /* ignore keep-alive comments / malformed frames */
      }
    };
    return () => es.close();
  }, [freqMs]);
}
