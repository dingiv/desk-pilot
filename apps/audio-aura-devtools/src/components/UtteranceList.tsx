import type { UtteranceItem } from '../types';
import { API_BASE } from '../apiBase';

interface Props {
  items: UtteranceItem[];
}

/**
 * Live list of Stage1 utterances. The last item is the sentence currently being recognized —
 * its `partial` streams char-by-char and earlier chars get rewritten as more audio arrives
 * (forward correction — inherent to how streaming ASR partials update). Finalized items show
 * the raw transcript + the Stage2-calibrated text + an intent badge.
 */
export function UtteranceList({ items }: Props) {
  return (
    <div className="va-conv" data-testid="utterance-list">
      {items.length === 0 && <div className="va-empty">等待识别…（确认上方已开启 scout 连接）</div>}
      {items.map((it) => {
        if (it.live) {
          // the in-progress sentence — streaming partial
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
        return (
          <div className="va-msg user" key={`final-${it.seq}`} data-testid="final-item">
            <div className="va-msg-role">
              你 #{it.seq}{' '}
              <span className={`va-badge ${f.intent}`} data-testid="intent-badge">
                {f.intent === 'task' ? '🛠️ 任务' : '💬 闲聊'}
              </span>
            </div>
            {corrected && <div className="va-raw" data-testid="raw">{f.raw}</div>}
            <div className="va-calibrated" data-testid="calibrated">
              {f.calibrated}
            </div>
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
          </div>
        );
      })}
    </div>
  );
}
