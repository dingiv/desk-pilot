import { useCallback, useEffect, useState } from 'react';
import { API_BASE } from './apiBase';
import { useAuraStream } from './hooks/useAuraStream';
import { useAuraSegments } from './hooks/useAuraSegments';
import { UtteranceList } from './components/UtteranceList';
import type { AsrSegment, AuraStateView, UtteranceItem } from './types';

/**
 * audio-aura dev UI — two-plane client:
 * - CONTROL plane: on mount + each `state_changed` ping → GET /api/state for the settings
 *   snapshot (connected / config / hotwords / corrections). Rendered into the header/status strip.
 * - DATA plane: `subscribe_segments` → each AsrSegment updates the local utterance list. The live
 *   recognition text comes straight off this (low-latency), not via re-fetching the snapshot.
 */
export default function App() {
  const [snap, setSnap] = useState<AuraStateView | null>(null); // settings (control plane)
  const [items, setItems] = useState<UtteranceItem[]>([]); // live list (data plane)

  // ── control plane ──
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

  // ── data plane: build the live list from recognition segments ──
  const onSegment = useCallback((seg: AsrSegment) => {
    setItems((prev) => {
      const i = prev.findIndex((it) => it.seq === seg.seq);
      // apply a mutation to the seq's item, creating a fresh live one if absent.
      const upsert = (mut: (u: UtteranceItem) => UtteranceItem): UtteranceItem[] => {
        if (i >= 0) {
          const cur = prev[i];
          if (cur) {
            const next = prev.slice();
            next[i] = mut(cur);
            return next;
          }
        }
        return [...prev, mut({ seq: seg.seq, partial: '', live: true })];
      };
      switch (seg.type) {
        case 'interim':
          return upsert((u) => ({ ...u, partial: seg.partial, live: true }));
        case 'calibrated_interim':
          return upsert((u) => ({ ...u, calibrated: seg.calibrated, live: true }));
        case 'final':
          return upsert((u) => ({
            ...u,
            live: false,
            final: {
              raw: seg.raw_text,
              streaming: seg.streaming_text,
              calibrated: seg.calibrated,
              intent: seg.intent,
              reply: seg.reply,
              route_ms: seg.route_ms,
            },
          }));
        case 'correction':
          return upsert((u) => ({ ...u, corrected_by_user: true }));
      }
    });
  }, []);
  useAuraSegments(onSegment);

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
          <UtteranceList utterances={items} />
        </section>
      </main>
      <footer className="aura-footer">
        {connected ? '正在识别…' : '录音已停止'} · {items.length} 句
      </footer>
    </div>
  );
}
