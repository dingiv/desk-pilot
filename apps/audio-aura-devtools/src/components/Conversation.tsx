import type { ConvItem } from '../types';

interface ConversationProps {
  items: ConvItem[];
  onPlayAudio: (chunkId: string) => void;
}

export function Conversation(props: ConversationProps) {
  return (
    <div className="va-conv" data-testid="conversation">
      {props.items.length === 0 && (
        <div className="va-empty">
          点上方 🎤 开始说话，或用下方"文本注入"喂一句话给小语。
          <br />
          试试："帮我把语音三阶段架构写成一篇技术博客" —— 看她判成任务并派写作 worker。
        </div>
      )}
      {props.items.map((it) => {
        if (it.kind === 'user') {
          return (
            <div className="va-msg user" key={it.id}>
              <div className="va-msg-role">你</div>
              {it.raw && it.raw !== it.calibrated && <div className="va-raw">{it.raw}</div>}
              <div className="va-calibrated" data-testid="calibrated">
                {it.calibrated || <span className="va-pending">整流中…</span>}
              </div>
              {it.hasAudio && (
                <button className="va-audio" onClick={() => props.onPlayAudio(it.chunkId)} title="重放原始录音">
                  ▶ 原声
                </button>
              )}
            </div>
          );
        }
        if (it.kind === 'secretary') {
          return (
            <div className="va-msg secretary" data-testid="secretary-msg" data-intent={it.intent} key={it.id}>
              <div className="va-msg-role">
                小语{' '}
                <span className={`va-badge ${it.intent}`} data-testid="intent-badge">
                  {it.intent === 'task' ? '🛠️ 任务' : '💬 闲聊'}
                </span>
              </div>
              <div className="va-reply">{it.reply}</div>
            </div>
          );
        }
        // task card
        return (
          <div className="va-task" data-testid="task-card" data-status={it.status} key={it.id}>
            <span className="va-task-icon">
              {it.status === 'running' ? '⏳' : it.status === 'done' ? '✅' : '❌'}
            </span>
            <span className="va-task-label">
              派发 worker「{it.capability}」·{' '}
              {it.status === 'running' ? '进行中…' : it.status === 'done' ? '已完成' : '失败'}
            </span>
          </div>
        );
      })}
    </div>
  );
}
