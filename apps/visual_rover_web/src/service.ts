/**
 * service.ts — 业务层
 *
 * Orchestrates agent tasks: wires the LLM/platform adapters from `./agent.js`, delegates
 * persistence to a {@link TaskStore}, and exposes one-shot (`execute`) and streaming
 * (`stream`) entry points. Routes calls this — it has zero HTTP knowledge.
 */
import type { Agent, TaskEvent } from '@vrover/agent';
import { MockPlatform } from '@vrover/platform';
import type { Platform } from '@vrover/platform';
import { createWebAgent } from './agent.js';
import { MemoryTaskStore, newTaskRecord } from './store.js';
import type { TaskRecord, TaskStore } from './store.js';
import { createLogger, type Logger } from '@vrover/logger';


export type { TaskRecord, TaskStore } from './store.js';

export class AgentService {
  logger: Logger;

  /** The shared brain every task is created from. Built lazily on first task — see {@link agent}. */
  private _agent?: Agent;

  /**
   * @param store  persistence layer
   * @param platform  the single target every task drives; defaults to an in-memory
   *   {@link MockPlatform}. Selected at boot (see `createPlatform` in `./agent.js`).
  */
  constructor(
    private readonly store: TaskStore,
    private readonly platform: Platform = new MockPlatform(),
  ) {
    this.logger = createLogger('web/service');
  }

  /**
   * The shared {@link Agent} — one brain driving many independent {@link Task}s (the documented
   * factory-for-tasks shape). **Lazy:** provider construction may throw without an API key, so the
   * Agent is created on first task, never at server boot (the rover app stays key-free to start).
   */
  private get agent(): Agent {
    return (this._agent ??= createWebAgent(this.platform));
  }

  /** Convenience: new service with an in-memory store + the default mock platform. */
  static create(): AgentService {
    const svc = new AgentService(new MemoryTaskStore());
    return svc
  }

  // ── one-shot ──────────────────────────────────────────────────────────────

  /**
   * Run an agent task to completion, persist the full record, and return it.
   * The caller blocks until the agent finishes (suitable for simple HTTP POST).
   */
  async execute(goal: string, maxSteps?: number): Promise<TaskRecord> {
    return this.runTask(goal, maxSteps, undefined);
  }

  // ── streaming ─────────────────────────────────────────────────────────────

  /**
   * Run an agent task with streaming progress. Each {@link TaskEvent} is forwarded to
   * `sink` (e.g. an SSE writer) AND persisted to the store incrementally.
   * Returns the final record after the task reaches a terminal state.
   */
  async stream(
    goal: string,
    maxSteps: number | undefined,
    sink: (ev: TaskEvent) => void,
  ): Promise<TaskRecord> {
    return this.runTask(goal, maxSteps, sink);
  }

  // ── shared task runner ────────────────────────────────────────────────────

  /**
   * Drive one task from the shared {@link Agent}. Every event is forwarded to `sink` (when given)
   * — that is how results reach the frontend (e.g. an SSE writer) — and applied to the record for
   * incremental persistence. The listener is always released in `finally`, so neither the record
   * nor the sink closure outlives the task.
   */
  private async runTask(
    goal: string,
    maxSteps: number | undefined,
    sink: ((ev: TaskEvent) => void) | undefined,
  ): Promise<TaskRecord> {
    const rec = newTaskRecord(goal);
    await this.store.save(rec);

    const task = this.agent.createTask(goal);
    const onEvent = (ev: TaskEvent): void => {
      sink?.(ev);
      this.applyEvent(rec, ev);
    };
    task.on(onEvent);

    try {
      rec.result = await task.run({ maxSteps });
    } catch (err) {
      // An observe/act throw escapes run() before any terminal event fires — synthesize one so the
      // sink (frontend) and the record both see the failure. (LLM errors are handled inside run()
      // and arrive here as a normal 'error' event, not a throw.)
      const msg = err instanceof Error ? err.message : String(err);
      this.logger.error('task failed', { goal, error: msg });
      onEvent({
        type: 'error',
        result: { status: 'error', error: msg, steps: [...task.steps] },
      });
    } finally {
      task.off(onEvent);
    }

    rec.status = rec.result?.status ?? 'error';
    rec.steps = [...task.steps];
    rec.updatedAt = Date.now();
    await this.store.save(rec);
    return rec;
  }

  // ── queries ───────────────────────────────────────────────────────────────

  async getTask(id: string): Promise<TaskRecord | null> {
    return this.store.load(id);
  }

  async listTasks(): Promise<TaskRecord[]> {
    return this.store.list();
  }

  // ── internal ──────────────────────────────────────────────────────────────

  private applyEvent(rec: TaskRecord, ev: TaskEvent): void {
    switch (ev.type) {
      case 'capture':
        // Captures are transient UI-only events (live screenshots); not part of the record.
        return;
      case 'log':
        if (ev.text) rec.log.push(ev.text);
        break;
      case 'step':
        if (ev.step) rec.steps.push(ev.step);
        break;
      case 'done':
      case 'error':
      case 'paused':
        if (ev.result) rec.result = ev.result;
        break;
    }
    rec.updatedAt = Date.now();
    this.store.save(rec); // fire-and-forget (store is in-memory, so no await needed)
  }
}
