interface HeaderProps {
  supported: boolean;
  listening: boolean;
  interim: string;
  sttError: string | null;
  ttsOn: boolean;
  topicTitle: string;
  onToggleMic: () => void;
  onToggleTts: () => void;
  onNewTopic: () => void;
  onRenameTopic: (title: string) => void;
}

export function Header(props: HeaderProps) {
  return (
    <header className="va-header">
      <div className="va-brand">
        <span className="va-logo">🎙️</span>
        <div>
          <div className="va-title">语音秘书 · 小语</div>
          <div className="va-sub">说话 → 整流 → 判断闲聊/任务 → 派 worker 干活</div>
        </div>
      </div>

      <div className="va-topic">
        <input
          className="va-topic-input"
          data-testid="topic-title"
          value={props.topicTitle}
          onChange={(e) => props.onRenameTopic(e.target.value)}
          aria-label="话题标题"
        />
        <button className="va-btn ghost" data-testid="new-topic" onClick={props.onNewTopic}>
          + 新话题
        </button>
      </div>

      <div className="va-controls">
        <button
          className={`va-btn tts ${props.ttsOn ? 'on' : ''}`}
          data-testid="tts-btn"
          onClick={props.onToggleTts}
          title="AI 语音朗读回复"
        >
          {props.ttsOn ? '🔊 朗读开' : '🔈 朗读关'}
        </button>
        <button
          className={`va-btn mic ${props.listening ? 'rec' : ''}`}
          data-testid="mic-btn"
          disabled={!props.supported}
          onClick={props.onToggleMic}
          title={props.supported ? '开始/停止说话' : '当前浏览器不支持语音识别'}
        >
          {props.listening ? '● 录音中…点此停止' : '🎤 开始说话'}
        </button>
      </div>

      {props.listening && props.interim && (
        <div className="va-interim" data-testid="interim">
          正在听：{props.interim}…
        </div>
      )}
      {!props.supported && (
        <div className="va-warn">
          此浏览器不支持 Web Speech 语音识别（需 Chrome 系）。你仍可用下方"文本注入"体验整条链路。
        </div>
      )}
      {props.sttError && <div className="va-warn">语音识别提示：{props.sttError}</div>}
    </header>
  );
}
