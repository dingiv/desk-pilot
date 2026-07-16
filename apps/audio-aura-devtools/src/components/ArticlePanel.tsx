import { marked } from 'marked';
import type { Topic } from '../types';

interface ArticlePanelProps {
  title: string;
  status: Topic['status'];
  markdown: string;
  generating: boolean;
  onGenerate: () => void;
}

const STATUS_LABEL: Record<Topic['status'], string> = {
  draft: '草稿',
  generating: '生成中…',
  complete: '已完成',
};

export function ArticlePanel(props: ArticlePanelProps) {
  const html = props.markdown ? (marked.parse(props.markdown, { async: false }) as string) : '';
  return (
    <div className="va-article" data-testid="article-panel">
      <div className="va-article-head">
        <div className="va-article-title">📄 {props.title}</div>
        <div className="va-article-actions">
          <span className={`va-pill ${props.status}`} data-testid="article-status">
            {STATUS_LABEL[props.status]}
          </span>
          <button
            className="va-btn primary"
            data-testid="generate-article"
            onClick={props.onGenerate}
            disabled={props.generating}
          >
            {props.generating ? '写作中…' : '✍️ 生成/重写文章'}
          </button>
        </div>
      </div>

      {html ? (
        <div className="va-md" data-testid="article-body" dangerouslySetInnerHTML={{ __html: html }} />
      ) : (
        <div className="va-empty">
          还没有文章。跟小语说"帮我把刚才聊的写成一篇……"，或点右上角「生成文章」，
          写作 worker 会把本话题里所有整流后的口述素材重排、润色成一篇结构化文章，实时流式出现在这里。
        </div>
      )}
    </div>
  );
}
