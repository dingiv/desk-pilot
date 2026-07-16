import { useCallback, useEffect, useState } from 'react';
import { API_BASE } from './apiBase';
import { useAsrStream } from './hooks/useAsrStream';
import { UtteranceList } from './components/UtteranceList';
import type { AsrEvent, UtteranceItem } from './types';

/**
 * audio-aura dev UI — wired to the Rust `aura-daemon` (NOT the legacy TS backend).
 *  - a toggle for aura's OWN scout connection (does NOT kill scout);
 *  - a live list of Stage1 utterances; the last item streams char-by-char with forward correction.
 */
export default function App() {
  const [connected, setConnected] = useState(false);
  const [items, setItems] = useState<UtteranceItem[]>([]);

  // initial connection state
  useEffect(() => {
    fetch(`${API_BASE}/api/status`)
      .then((r) => r.json())
      .then((b: { connected?: boolean }) => setConnected(!!b.connected))
      .catch(() => setConnected(false));
  }, []);

  const onEvent = useCallback((ev: AsrEvent) => {
    switch (ev.type) {
      case 'hello':
        break;
      case 'status':
        setConnected(ev.connected);
        break;
      case 'interim':
        // update the live item with this seq (create it if a new utterance started)
        setItems((prev) => {
          const i = prev.findIndex((it) => it.seq === ev.seq);
          if (i >= 0) {
            const next = prev.slice();
            const cur = next[i];
            if (cur) next[i] = { ...cur, partial: ev.partial, live: true };
            return next;
          }
          return [...prev, { seq: ev.seq, partial: ev.partial, live: true }];
        });
        break;
      case 'final':
        // freeze the item with this seq (set final, mark not live)
        setItems((prev) => {
          const i = prev.findIndex((it) => it.seq === ev.seq);
          if (i < 0) return prev;
          const next = prev.slice();
          const cur = next[i];
          if (!cur) return prev;
          next[i] = {
            ...cur,
            live: false,
            final: {
              raw: ev.raw_text,
              streaming: ev.streaming_text,
              calibrated: ev.calibrated,
              intent: ev.intent,
              reply: ev.reply,
              route_ms: ev.route_ms,
            },
          };
          return next;
        });
        break;
    }
  }, []);
  useAsrStream(onEvent);

  const toggle = useCallback(() => {
    const enabled = !connected;
    setConnected(enabled); // optimistic; the `status` event will confirm
    fetch(`${API_BASE}/api/control/scout`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ enabled }),
    }).catch(() => setConnected(!enabled));
  }, [connected]);

  return (
    <div className="va-app">
      <header className="va-header aura-header">
        <div className="aura-brand">
          <span className={`aura-dot ${connected ? 'on' : 'off'}`} title={connected ? '已连接 scout' : '未连接'} />
          audio-aura
        </div>
        <button
          className={`aura-toggle ${connected ? 'on' : 'off'}`}
          onClick={toggle}
          data-testid="scout-toggle"
        >
          {connected ? '⏹ 停止录音' : '▶ 开始录音'}
        </button>
      </header>
      <main className="va-main">
        <section className="va-left">
          <UtteranceList items={items} />
        </section>
      </main>
      <footer className="aura-footer">
        {connected ? '正在识别…' : '录音已停止'} · {items.length} 句
      </footer>
    </div>
  );
}
