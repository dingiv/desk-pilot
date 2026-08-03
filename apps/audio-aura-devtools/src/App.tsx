import { useCallback, useEffect, useState } from 'react';
import { API_BASE } from './apiBase';
import { useAuraStream } from './hooks/useAuraStream';
import { UtteranceList } from './components/UtteranceList';
import type { AuraStateView } from './types';

/**
 * audio-aura dev UI — snapshot-sync against the Rust `aura-daemon`.
 * - On mount + on every `state_changed` ping (GET /api/stream, throttled ≥250ms): GET /api/state
 *   for the full `AuraStateView` and render it. The daemon is the single source of truth; this
 *   component holds no derived state of its own.
 * - A toggle for aura's OWN scout connection (does NOT kill scout); a config/status strip.
 */
export default function App() {
  const [snap, setSnap] = useState<AuraStateView | null>(null);

  // Pull the full snapshot. Called on mount and on each state_changed ping.
  const refresh = useCallback(() => {
    fetch(`${API_BASE}/api/state`)
      .then((r) => r.json())
      .then((s: AuraStateView) => setSnap(s))
      .catch(() => {});
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);
  useAuraStream(refresh);

  const connected = snap?.connected ?? false;
  const toggle = useCallback(() => {
    const enabled = !connected;
    fetch(`${API_BASE}/api/control/scout`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ enabled }),
    })
      .then(() => refresh())
      .catch(() => {});
  }, [connected, refresh]);

  const cfg = snap?.config;
  const vad = cfg?.vad;

  return (
    <div className="va-app">
      <header className="va-header aura-header">
        <div className="aura-brand">
          <span
            className={`aura-dot ${connected ? 'on' : 'off'}`}
            title={connected ? '已连接 scout' : '未连接'}
          />
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

      {cfg && (
        <div className="aura-status" data-testid="status-strip">
          ASR {cfg.asr_backend}({cfg.asr_provider}) · LLM {cfg.llm_kind}:{cfg.model} · VAD
          merge_gap {vad ? vad.merge_gap : '?'}s · 热词 {snap?.hotwords.length ?? 0} ·
          {snap?.stage3_on ? ' Stage3' : ' no-Stage3'}
        </div>
      )}

      <main className="va-main">
        <section className="va-left">
          <UtteranceList utterances={snap?.utterances ?? []} />
        </section>
      </main>
      <footer className="aura-footer">
        {connected ? '正在识别…' : '录音已停止'} · {snap?.utterances.length ?? 0} 句
      </footer>
    </div>
  );
}
