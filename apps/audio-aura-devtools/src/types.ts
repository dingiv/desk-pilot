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

// ── daemon (Rust) SSE + state contract ────────────────────────────────────
// GET /api/stream?state_changed_frequency=<ms> emits `hello`, then `state_changed` pings
// (throttled to ≥250ms). On a ping — and on mount — the frontend GETs /api/state for the full
// `AuraStateView` snapshot and re-renders. One source of truth, one fetch (snapshot-sync).
export type StreamPing = { type: 'hello' } | { type: 'state_changed' };

export interface VadView {
  threshold: number;
  min_silence: number;
  merge_gap: number;
}
export interface ConfigView {
  asr_backend: string;
  asr_kind: string; // local | remote
  asr_provider: string; // cpu | cuda
  llm_kind: string;
  model: string;
  vad: VadView;
}
export interface CorrectionView {
  raw: string;
  corrected: string;
}
export interface FinalView {
  raw: string;
  streaming: string;
  calibrated: string;
  intent: string;
  reply: string;
  route_ms: number;
}
/// One utterance in the timeline (live or finalized).
export interface UtteranceView {
  seq: number;
  /** Latest streaming partial (raw, live). */
  partial: string;
  /** Stage2's provisional calibration (per fragment) — shown in preference to `partial` when set. */
  calibrated?: string;
  /** Set when the utterance settled (VAD settle + Stage2 final calibration). */
  final?: FinalView;
  /** Still being recognized (absorbing fragments). */
  live: boolean;
  /** Set when the user corrected this utterance via POST /api/correct. */
  corrected_by_user?: boolean;
  at_s: number;
}
/// The complete state snapshot served by GET /api/state.
export interface AuraStateView {
  connected: boolean;
  stage3_on: boolean;
  config: ConfigView;
  hotwords: string[];
  corrections: CorrectionView[];
  utterances: UtteranceView[];
}
