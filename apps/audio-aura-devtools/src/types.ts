// 与后端 SSE / REST 对应的前端类型。

export interface Topic {
  topic_id: string;
  title: string;
  article_markdown: string;
  status: 'draft' | 'generating' | 'complete';
  created_at: number;
  updated_at: number;
}

export interface CalibratedNode {
  node_id: string;
  linked_chunks: string;
  calibrated_text: string;
  topic_id: string | null;
  created_at: number;
}

export type StreamEvent =
  | { type: 'hello' }
  | { type: 'chunk'; chunk_id: string; raw_text: string; topic_id: string; has_audio: boolean }
  | {
      type: 'node';
      node_id: string;
      calibrated_text: string;
      linked_chunks: string[];
      merged: boolean;
      topic_id: string;
    }
  | { type: 'secretary'; intent: 'chat' | 'task'; reply: string; task_id: string | null; topic_id: string }
  | { type: 'task'; task_id: string; capability: string; status: 'running' | 'done' | 'failed'; topic_id: string }
  | { type: 'article_delta'; topic_id: string; text: string }
  | { type: 'article_done'; topic_id: string; article_md: string }
  | { type: 'error'; scope?: string; message: string; topic_id?: string };

// ── conversation timeline items ──────────────────────────────────────────
export interface UserTurnItem {
  kind: 'user';
  id: string;
  chunkId: string;
  raw: string;
  calibrated: string;
  hasAudio: boolean;
}
export interface SecretaryItem {
  kind: 'secretary';
  id: string;
  intent: 'chat' | 'task';
  reply: string;
  taskId: string | null;
}
export interface TaskItem {
  kind: 'task';
  id: string;
  taskId: string;
  capability: string;
  status: 'running' | 'done' | 'failed';
}
export type ConvItem = UserTurnItem | SecretaryItem | TaskItem;

// ── daemon (Rust) SSE contract: live Stage1 recognition ───────────────────
// Emitted by aura-daemon GET /api/stream. `interim` updates the current (in-progress)
// utterance; `final` freezes it (with calibration) and the next interim starts a new one.
export type AsrEvent =
  | { type: 'hello' }
  | { type: 'status'; connected: boolean }
  | { type: 'interim'; seq: number; partial: string; at_s: number }
  | {
      type: 'final';
      seq: number;
      raw_text: string;
      streaming_text: string;
      calibrated: string;
      intent: string;
      reply: string;
      route_ms: number;
    };

/// One Stage1 utterance in the live list. `live` = currently being recognized (partial streams
/// char-by-char, earlier chars get corrected as more audio arrives — forward correction).
export interface UtteranceItem {
  seq: number;
  /** Latest streaming partial (live) — the evolving best hypothesis. */
  partial: string;
  /** Set when the utterance is finalized (VAD end-of-speech + Stage2 calibration done). */
  final?: {
    raw: string;
    streaming: string;
    calibrated: string;
    intent: string;
    reply: string;
    route_ms: number;
  };
  live: boolean;
}
