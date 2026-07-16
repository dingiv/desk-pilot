import { useState } from 'react';

interface DevInjectProps {
  onInject: (text: string) => void;
}

/** Dev-only：无麦克风也能把一句"识别原文"喂进 Stage1→2→秘书→worker，供自测/Playwright 驱动。 */
export function DevInject(props: DevInjectProps) {
  const [text, setText] = useState('');
  const submit = () => {
    const t = text.trim();
    if (!t) return;
    props.onInject(t);
    setText('');
  };
  return (
    <div className="va-dev">
      <span className="va-dev-label">🧪 文本注入（当作一句语音识别结果）</span>
      <textarea
        className="va-dev-input"
        data-testid="dev-input"
        value={text}
        placeholder="例：帮我把刚才聊的语音三阶段架构写成一篇技术博客"
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) submit();
        }}
        rows={2}
      />
      <button className="va-btn" data-testid="dev-inject" onClick={submit}>
        注入 ⌘/Ctrl+↵
      </button>
    </div>
  );
}
