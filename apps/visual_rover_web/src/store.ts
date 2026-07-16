/**
 * store.ts — 持久层
 *
 * Records every completed agent run so the web UI can list past tasks and drill into
 * their step-by-step details. The {@link TaskStore} interface is the seam: swap in a
 * file-backed or SQLite store later without touching routes or service.
 */
import { randomUUID } from 'node:crypto';
import type { AgentStep, TaskResult } from '@vrover/agent';

// ── record ────────────────────────────────────────────────────────────────────

export interface TaskRecord {
  id: string;
  goal: string;
  status: string;
  steps: AgentStep[];
  /** Every progress log line emitted during the run. */
  log: string[];
  /** Terminal result; set once the task reaches done/error/paused. */
  result?: TaskResult;
  createdAt: number;
  updatedAt: number;
}

// ── store interface ───────────────────────────────────────────────────────────

export interface TaskStore {
  save(record: TaskRecord): Promise<void>;
  load(id: string): Promise<TaskRecord | null>;
  list(): Promise<TaskRecord[]>;
  remove(id: string): Promise<void>;
}

// ── in-memory implementation ──────────────────────────────────────────────────

export class MemoryTaskStore implements TaskStore {
  private readonly records = new Map<string, TaskRecord>();

  async save(record: TaskRecord): Promise<void> {
    this.records.set(record.id, record);
  }

  async load(id: string): Promise<TaskRecord | null> {
    return this.records.get(id) ?? null;
  }

  async list(): Promise<TaskRecord[]> {
    return [...this.records.values()].sort((a, b) => b.createdAt - a.createdAt);
  }

  async remove(id: string): Promise<void> {
    this.records.delete(id);
  }
}

// ── helpers ───────────────────────────────────────────────────────────────────

export function newTaskRecord(goal: string, id?: string): TaskRecord {
  const now = Date.now();
  return {
    id: id ?? randomUUID(),
    goal,
    status: 'running',
    steps: [],
    log: [],
    createdAt: now,
    updatedAt: now,
  };
}
