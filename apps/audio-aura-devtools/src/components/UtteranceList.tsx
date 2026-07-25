import { useState } from 'react';
import type { UtteranceItem } from '../types';
import { API_BASE } from '../apiBase';

interface Props {
  items: UtteranceItem[];
}

/**
 * Live list of Stage1 utterances. The last item is the sentence currently being recognized —
 * its `partial` streams char-by-char and earlier chars get rewritten as more audio arrives.
 * Finalized items show the raw transcript + the Stage2-calibrated text + an intent badge,
 * and have a "✏️ 纠正" button for inline editing → POST /api/correct.
 */
export function UtteranceList({ items }: Props) {
  const [editingSeq, setEditingSeq] = useState<number | null>(null);
  const [editText, setEditText] = useState('');

  const submitCorrection = (seq: number, raw: string) => {
    fetch(`${API_BASE}/api/correct`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ seq, raw, corrected: editText }),
    }).catch(() => {});
    setEditingSeq(null);
    setEditText('');
  };

  return (
    <div className="va-conv" data-testid="utterance-list">
      {items.length === 0 && <div className="va-empty">等待识别…（确认上方已开启 scout 连接）</div>}
      {items.map((it) => {
        if (it.live) {
          return (
            <div className="va-msg user aura-live" key={`live-${it.seq}`} data-testid="live-item">
              <div className="va-msg-role">
                你 <span className="aura-typing">识别中</span>
              </div>
              <div className="va-calibrated" data-testid="partial">
                {it.partial || <span className="va-pending">…</span>}
                <span className="aura-caret">▌</span>
              </div>
            </div>
          );
        }
        const f = it.final!;
        const corrected = f.raw && f.raw !== f.calibrated;
        const isEditing = editingSeq === it.seq;
        return (
          <div className="va-msg user" key={`final-${it.seq}`} data-testid="final-item">
            <div className="va-msg-role">
              你 #{it.seq}{' '}
              <span className={`va-badge ${f.intent}`} data-testid="intent-badge">
                {f.intent === 'task' ? '🛠️ 任务' : '💬 闲聊'}
              </span>
              {it.corrected_by_user && (
                <span className="va-badge corrected" title="用户已纠正">✓ 已纠正</span>
              )}
            </div>
            {corrected && !isEditing && <div className="va-raw" data-testid="raw">{f.raw}</div>}
            {isEditing ? (
              <div className="va-edit-row">
                <input
                  className="va-edit-input"
                  value={editText}
                  onChange={(e) => setEditText(e.target.value)}
                  autoFocus
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') submitCorrection(it.seq, f.raw);
                    if (e.key === 'Escape') { setEditingSeq(null); setEditText(''); }
                  }}
                />
                <button className="va-edit-btn" onClick={() => submitCorrection(it.seq, f.raw)}>✓</button>
                <button
                  className="va-edit-btn cancel"
                  onClick={() => { setEditingSeq(null); setEditText(''); }}
                >
                  ✕
                </button>
              </div>
            ) : (
              <div className="va-calibrated" data-testid="calibrated">{f.calibrated}</div>
            )}
            {!isEditing && (
              <div className="va-actions">
                <button
                  className="va-audio"
                  onClick={() => {
                    const audio = new Audio(`${API_BASE}/api/audio/${it.seq}`);
                    audio.play().catch(() => {});
                  }}
                  title="播放原声"
                >
                  ▶ 原声
                </button>
                <button
                  className="va-edit-trigger"
                  onClick={() => { setEditingSeq(it.seq); setEditText(f.calibrated); }}
                  title="编辑纠正"
                >
                  ✏️ 纠正
                </button>
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
