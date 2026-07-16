/**
 * team.ts — the multi-agent execution layer: team loop · write lock · DeliverTask.
 *
 * Additive over the existing single-agent GUI loop (`core.ts`): that loop stays untouched and is
 * reused as the GUI worker's `tick` (see {@link createGUIAgent}). This module adds the top-level
 * cooperative scheduler ({@link TeamLoop}), the leader / worker agent roles, and the
 * `deliver_task` delegation primitive with suspend / resume + mechanical completion.
 *
 * Status: first revision. Deferred to later revisions (see `docs/execution-model.md` §9): the
 * resource / lease layer (exclusive desktop, Platform-internal-to-tool), MCP / skills / config,
 * `ContextManager` / `PromptBuilder`, the `Observes` / `Acts` / `Grounds` action interfaces, and
 * removing `run` / `exec` from `Task`. Everything here is key-free / network-free — it drives
 * injected `CompleteFn` fakes, exactly like the rest of the brain.
 */
import type { CompleteFn, ContentBlock, ToolDef } from '@vrover/llm';
import { MockPlatform } from '@vrover/platform';
import type { Platform } from '@vrover/platform';
import type { Agent, AgentStep, Task, TaskResult, TaskStatus, TaskSuspendState } from './types.js';
import { createAgent } from './core.js';
import { createResourceManager } from './resources.js';
import type { Lease, Resource, ResourceManager } from './resources.js';
import type { GroundingModel } from './model.js';
import { captureObservation, performPlatformAction } from './actions.js';

// ── DeliverTask tool ─────────────────────────────────────────────────────────

/** Input the leader passes when delegating a subtask to a worker. */
export interface DeliverTaskInput {
  /** Target worker agent id (resolved against the team roster). */
  readonly to: string;
  /** The subtask goal (becomes the worker task's goal). */
  readonly goal: string;
  /** Optional context / constraints from the leader. */
  readonly context?: string;
}

/** A delegated subtask's terminal result, projected for the leader to read. */
export interface DeliverTaskResult {
  readonly subtaskId: string;
  readonly status: TaskStatus;
  readonly output: string;
  readonly summary?: string;
}

/** What the team loop appends back into a suspended leader's history when its subtask finishes. */
export interface DelegateResolution {
  readonly parentTaskId: string;
  readonly toolUseId: string;
  readonly result: DeliverTaskResult;
}

/** The leader's delegation tool: hand a subtask to a worker and await its result. */
export const DELIVER_TASK_TOOL: ToolDef = {
  name: 'deliver_task',
  description:
    'Delegate a subtask to a worker agent. You will receive its result before continuing. ' +
    'You do not operate the GUI yourself — delegate that to a GUI worker.',
  input_schema: {
    type: 'object',
    properties: {
      to: { type: 'string', description: 'target worker agent id' },
      goal: { type: 'string', description: 'the subtask goal' },
      context: { type: 'string', description: 'optional context / constraints for the worker' },
    },
    required: ['to', 'goal'],
  },
};

/** The leader's completion tool: mark the whole task done with a summary. */
const FINISH_TOOL: ToolDef = {
  name: 'finish',
  description: 'Mark the whole task complete with a summary.',
  input_schema: {
    type: 'object',
    properties: { summary: { type: 'string' } },
    required: ['summary'],
  },
};

// ── tick outcome ─────────────────────────────────────────────────────────────

/** One delegation a leader emits in a tick (the team loop materializes it into a worker subtask). */
export interface DelegateIntent {
  readonly toolUseId: string;
  readonly workerId: string;
  readonly goal: string;
  readonly context?: string;
}

/**
 * One tick's result (pure data). The team loop uses it to update scheduling state. Worker ticks
 * (`createGUIAgent`) return `progress` / `done` / `error`; the task's own `status` already reflects
 * the terminal cases (set by `exec`). Leader ticks may additionally return `suspended` — with one
 * or more `delegates` (fan-out): the leader parks until **all** of them finish (wait-for-all).
 */
export type TickOutcome =
  | { kind: 'progress'; step?: AgentStep }
  | { kind: 'suspended'; delegates: readonly DelegateIntent[] }
  | { kind: 'done'; summary?: string }
  | { kind: 'error'; error: string };

// ── profiles / roster ────────────────────────────────────────────────────────

/** Pure-data capability declaration + routing basis. */
export interface AgentProfile {
  readonly id: string;
  readonly role: 'leader' | 'worker';
  readonly specialties: readonly string[];
  /** Human / model-readable capability declaration. Injected into the LEADER's prompt only. */
  readonly bio: string;
  /** Exclusive resource ids this agent must hold (via lease) before its task may tick. */
  readonly requires?: readonly string[];
}

/** The team's worker roster — visible only to the leader's prompt builder. */
export interface TeamRoster {
  readonly workers: readonly AgentProfile[];
}

// ── agents ───────────────────────────────────────────────────────────────────

/**
 * A team agent. Each `tick(task)` advances one unit of that agent's loop on `task`;
 * `createTask` mints a task owned by this agent (wired to its collaborators). The team loop is the
 * only caller of `tick`; the per-agent tick shape is how agent roles differ (see `docs/execution-model.md` §2).
 */
export interface TeamAgent {
  readonly profile: AgentProfile;
  /** Mint a task owned by this agent (ownerId recorded on the task). */
  createTask(goal: string, opts?: { id?: string; ownerId?: string }): Task;
  /** Advance `task` by one unit of this agent's loop. */
  tick(task: Task): Promise<TickOutcome>;
}

/** Leader: plans + delegates via `deliver_task`; never observes (vision is the GUI worker's job). */
export interface LeaderAgent extends TeamAgent {}

/** GUI worker: its tick is the existing observe → think → act loop (`core.ts` `exec`). */
export interface GUIAgent extends TeamAgent {}

// ── leader system prompt ─────────────────────────────────────────────────────

function renderLeaderSystem(roster: TeamRoster): string {
  const workers = roster.workers
    .map((w) => `- ${w.id} [${w.specialties.join(', ')}]: ${w.bio}`)
    .join('\n');
  return [
    'You are the team leader. Decompose the goal and delegate each piece to a worker with the',
    '`deliver_task` tool, then call `finish` with a summary when the whole goal is complete.',
    'You do not capture screenshots or operate the GUI yourself — that is a worker specialty.',
    'Wait for each delegation to return before continuing.',
    '',
    'Workers:',
    workers || '- (none)',
  ].join('\n');
}

export interface LeaderAgentDeps {
  readonly profile: AgentProfile;
  readonly complete: CompleteFn;
  readonly roster: TeamRoster;
  readonly systemPrompt?: string;
  readonly maxSteps?: number;
}

/**
 * Build a leader agent. Its tick = `complete()` + `deliver_task` / `finish`; it never captures the
 * screen. A core `Agent` is held only to mint tasks (`createTask`) — its platform / SoM loop are
 * never driven (the leader tick mutates the task via the external-driver seam instead).
 */
export function createLeaderAgent(deps: LeaderAgentDeps): LeaderAgent {
  const system = deps.systemPrompt ?? renderLeaderSystem(deps.roster);
  const core = createAgent({
    platform: new MockPlatform(),
    complete: deps.complete,
    tools: [DELIVER_TASK_TOOL, FINISH_TOOL],
    systemPrompt: system,
    maxSteps: deps.maxSteps ?? 100,
  });

  const tick = async (task: Task): Promise<TickOutcome> => {
    const resp = await deps.complete({
      system,
      messages: [...task.history],
      tools: [DELIVER_TASK_TOOL, FINISH_TOOL],
    });
    const raw: ContentBlock[] =
      resp.raw.length > 0 ? resp.raw : [{ type: 'text', text: resp.text ?? '' }];
    task.append({ role: 'assistant', content: raw });

    if (resp.toolUses.length === 0) {
      task.append({
        role: 'user',
        content: [{ type: 'text', text: 'Call deliver_task to delegate, or finish when done.' }],
      });
      return { kind: 'progress' };
    }

    const delegates: DelegateIntent[] = [];
    let finishSummary: string | undefined;
    for (const tu of resp.toolUses) {
      if (tu.name === 'deliver_task') {
        const input = tu.input as unknown as DeliverTaskInput;
        delegates.push({
          toolUseId: tu.id,
          workerId: input.to,
          goal: input.goal,
          context: input.context,
        });
      } else if (tu.name === 'finish') {
        finishSummary = typeof tu.input.summary === 'string' ? tu.input.summary : undefined;
      } else {
        task.append({
          role: 'user',
          content: [
            {
              type: 'tool_result',
              tool_use_id: tu.id,
              content: `unknown tool: ${tu.name}`,
              is_error: true,
            },
          ],
        });
      }
    }
    // Fan-out: suspend on every deliver_task emitted this tick (wait-for-all before resuming).
    if (delegates.length > 0) {
      return { kind: 'suspended', delegates };
    }
    return { kind: 'done', summary: finishSummary };
  };

  return {
    profile: deps.profile,
    createTask: (goal, opts) => core.createTask(goal, { id: opts?.id, ownerId: opts?.ownerId }),
    tick,
  };
}

export interface GUIAgentDeps {
  readonly profile: AgentProfile;
  /** A wired core GUI Agent (the observe → think → act loop). The worker's tasks are minted from it. */
  readonly core: Agent;
}

/**
 * Bridge a core GUI Agent (the existing observe → think → act loop) into a team worker. Its `tick`
 * runs exactly one `task.exec()` and maps the task's resulting status to a `TickOutcome`. The SoM
 * loop stays exactly as implemented in `core.ts` — this is the GUI worker's tick.
 */
export function createGUIAgent(deps: GUIAgentDeps): GUIAgent {
  const tick = async (task: Task): Promise<TickOutcome> => {
    const step = await task.exec();
    // exec() has already set status + result for terminal cases; the loop reads status directly.
    if (task.status === 'done') return { kind: 'done', summary: task.result?.summary };
    if (task.status === 'error') return { kind: 'error', error: task.result?.error ?? 'error' };
    return { kind: 'progress', step: step ?? undefined };
  };
  return {
    profile: deps.profile,
    createTask: (goal, opts) => deps.core.createTask(goal, { id: opts?.id, ownerId: opts?.ownerId }),
    tick,
  };
}

export interface GroundingAgentDeps {
  readonly profile: AgentProfile;
  /** A pixel-grounding model (GUI-TARS class) — emits actions directly from a screenshot + hint. */
  readonly model: GroundingModel;
  /** The desktop tool surface (Platform) this worker captures + acts through. */
  readonly platform: Platform;
}

/**
 * A GUI worker that grounds on **pixels** via a `Grounds` model (GUI-TARS class), bypassing SoM/mark
 * entirely (design.md §3 scenario two). Each tick: capture → `model.ground(obs, goal)` → perform the
 * returned pixel action directly; loop until the model returns `done`. The hint each tick is the
 * task goal. Composes `Observes` (capture-only) + `Grounds` + `Acts` (pixel-direct).
 */
export function createGroundingAgent(deps: GroundingAgentDeps): GUIAgent {
  // A core Agent is held only to mint tasks (createTask); its chat path is never driven.
  const core = createAgent({
    platform: deps.platform,
    complete: async () => {
      throw new Error('grounding agent has no chat model; it grounds via model.ground()');
    },
    tools: [],
  });
  const tick = async (task: Task): Promise<TickOutcome> => {
    const obs = await captureObservation(deps.platform);
    const action = await deps.model.ground(obs, task.goal);
    const r = await performPlatformAction(action, deps.platform);
    if (r.finished) {
      task.setResult({ status: 'success', summary: r.message, steps: [] });
      task.markStatus('done');
      return { kind: 'done', summary: r.message };
    }
    return { kind: 'progress' };
  };
  return {
    profile: deps.profile,
    createTask: (goal, opts) => core.createTask(goal, { id: opts?.id, ownerId: opts?.ownerId }),
    tick,
  };
}

// ── AgentTeam + TeamLoop ─────────────────────────────────────────────────────

/** The multi-agent team: resource pool + task factory + the scheduler (team loop). */
export interface AgentTeam {
  readonly loop: TeamLoop;
  readonly agents: ReadonlyMap<string, TeamAgent>;
  readonly tasks: ReadonlyMap<string, Task>;
  readonly roster: TeamRoster;
  /** The single active leader's agent id (one leader per team). */
  readonly leaderId: string;
  /** The resource pool + lease bookkeeping (shared tools + exclusive desktop). */
  readonly resources: ResourceManager;
  /** Mint a task owned by `ownerId` (default: the leader). Registered with the team. */
  createTask(goal: string, opts?: { id?: string; ownerId?: string }): Task;
  /** Run a root (leader) task to a terminal state under the team loop. */
  run(task: Task, opts?: { maxRounds?: number }): Promise<TaskResult>;
}

/**
 * The cooperative-pump scheduler. Each `round()` advances every runnable task by one tick
 * (leader first, then workers concurrently); mechanical completion feeds finished subtask results
 * back into suspended leaders. `run()` loops `round()` until the root task terminates.
 */
export interface TeamLoop {
  /** Advance one scheduler round. */
  round(): Promise<void>;
  /** Auto-pilot `round()` until the root task reaches a terminal state. */
  run(rootTaskId: string, opts?: { maxRounds?: number }): Promise<TaskResult>;
  /** Cooperatively stop after the current round. */
  stop(): void;
}

export interface AgentTeamDeps {
  /** The single active leader. */
  readonly leader: LeaderAgent;
  readonly workers: readonly TeamAgent[];
  /** Pool resources (shared tools + exclusive desktops). Defaults to none. */
  readonly resources?: readonly Resource[];
}

/** Wire a team: one leader + N workers + a resource pool, plus its own bound team loop. */
export function createAgentTeam(deps: AgentTeamDeps): AgentTeam {
  const agents = new Map<string, TeamAgent>();
  agents.set(deps.leader.profile.id, deps.leader);
  for (const w of deps.workers) agents.set(w.profile.id, w);
  const roster: TeamRoster = { workers: deps.workers.map((w) => w.profile) };
  const tasks = new Map<string, Task>();
  const resources = createResourceManager(deps.resources ?? []);
  const loop = new TeamLoopImpl();
  const leaderId = deps.leader.profile.id;

  const team: AgentTeam = {
    get loop() {
      return loop;
    },
    get agents() {
      return agents;
    },
    get tasks() {
      return tasks;
    },
    get roster() {
      return roster;
    },
    get leaderId() {
      return leaderId;
    },
    get resources() {
      return resources;
    },
    createTask(goal, opts) {
      const ownerId = opts?.ownerId ?? leaderId;
      const owner = agents.get(ownerId);
      if (!owner) throw new Error(`createTask: unknown owner agent '${ownerId}'`);
      const task = owner.createTask(goal, { id: opts?.id, ownerId });
      tasks.set(task.id, task);
      return task;
    },
    run(task, opts) {
      return loop.run(task.id, opts);
    },
  };
  loop.bind(team);
  return team;
}

// ── TeamLoop implementation ──────────────────────────────────────────────────

function isTerminal(t: Task): boolean {
  return t.status === 'done' || t.status === 'error';
}

class TeamLoopImpl implements TeamLoop {
  private team!: AgentTeam;
  private readonly inFlight = new Set<string>();
  private readonly taskLeases = new Map<string, Lease[]>();
  private stopped = false;

  /** Late-bound by `createAgentTeam` (the loop needs the team to query tasks / agents). */
  bind(team: AgentTeam): void {
    this.team = team;
  }

  stop(): void {
    this.stopped = true;
  }

  async run(rootTaskId: string, opts?: { maxRounds?: number }): Promise<TaskResult> {
    const maxRounds = opts?.maxRounds ?? 1000;
    let rounds = 0;
    while (!this.stopped) {
      const root = this.team.tasks.get(rootTaskId);
      if (!root) throw new Error(`team run: unknown root task '${rootTaskId}'`);
      if (isTerminal(root)) {
        return root.result ?? { status: 'max_steps', steps: root.steps.slice() };
      }
      if (rounds++ >= maxRounds) {
        throw new Error(`team loop exceeded ${maxRounds} rounds (root task not done)`);
      }
      await this.round();
    }
    const root = this.team.tasks.get(rootTaskId);
    return root?.result ?? { status: 'max_steps', steps: root?.steps.slice() ?? [] };
  }

  async round(): Promise<void> {
    // 1. Mechanical completion: feed finished subtask results back to suspended leaders.
    this.resolveDelegations();
    // 2. Leader first (it is the work source; running it emits deliver_task to feed workers).
    const leaderTask = this.leaderTask();
    if (leaderTask && this.acquireRequired(leaderTask)) await this.tickOnce(leaderTask);
    // 3. Workers (runnable, non-leader) — concurrent: independent tasks share no write lock.
    //    Each must hold (or just acquire) its required exclusive resources, else it blocks this round.
    const workerTasks = [...this.team.tasks.values()].filter(
      (t) =>
        t.ownerId !== this.team.leaderId &&
        this.isTickable(t) &&
        this.acquireRequired(t),
    );
    if (workerTasks.length > 0) await Promise.all(workerTasks.map((t) => this.tickOnce(t)));
    // 4. Release leases held by tasks that just reached a terminal state.
    this.releaseTerminated();
  }

  /** Advance one task by one tick (write-lock: one in-flight tick per task — enforced by `inFlight`). */
  private async tickOnce(task: Task): Promise<void> {
    if (!this.isTickable(task)) return;
    const owner = this.team.agents.get(task.ownerId);
    if (!owner) {
      throw new Error(`tickOnce: no agent owns task '${task.id}' (ownerId='${task.ownerId}')`);
    }
    this.inFlight.add(task.id);
    try {
      const outcome = await owner.tick(task);
      this.applyOutcome(task, outcome);
    } finally {
      this.inFlight.delete(task.id);
    }
  }

  /**
   * Apply a tick's outcome to scheduling state. The team loop is the single writer of status /
   * result / suspend for externally-driven (leader) tasks; worker terminal status is already set by
   * `exec`, so the `!isTerminal` guards avoid clobbering the worker's own result.
   */
  private applyOutcome(task: Task, outcome: TickOutcome): void {
    switch (outcome.kind) {
      case 'suspended': {
        // Materialize one subtask per delegate, then park the leader on the whole batch.
        const states: TaskSuspendState[] = [];
        for (const d of outcome.delegates) {
          const sub = this.team.createTask(d.goal, { ownerId: d.workerId });
          if (d.context) {
            sub.append({ role: 'user', content: [{ type: 'text', text: d.context }] });
          }
          states.push({ toolUseId: d.toolUseId, subtaskId: sub.id, workerId: d.workerId });
        }
        task.setSuspend(states);
        task.markStatus('suspended');
        break;
      }
      case 'done': {
        if (!isTerminal(task)) {
          task.setResult({ status: 'success', summary: outcome.summary, steps: [] });
          task.markStatus('done');
        }
        break;
      }
      case 'error': {
        if (!isTerminal(task)) {
          task.setResult({ status: 'error', error: outcome.error, steps: [] });
          task.markStatus('error');
        }
        break;
      }
      case 'progress':
        if (task.status === 'idle') task.markStatus('running');
        break;
    }
  }

  /**
   * Feed finished delegations' results back into suspended leaders. A leader resumes only when
   * **every** pending delegation has terminated (wait-for-all); then each result is appended as its
   * own `tool_result` and the leader is made runnable again.
   */
  private resolveDelegations(): void {
    for (const task of this.team.tasks.values()) {
      if (task.status !== 'suspended' || task.suspendedOn.length === 0) continue;
      const pending = task.suspendedOn;
      const subs = pending.map((p) => this.team.tasks.get(p.subtaskId));
      if (subs.some((s) => !s || !isTerminal(s))) continue; // not all finished yet
      // Batch all tool_results into one user message (deepseek requires each
      // assistant tool_use to have a matching tool_result in the very next message).
      const results: ContentBlock[] = [];
      for (let i = 0; i < pending.length; i++) {
        const p = pending[i]!;
        const sub = subs[i]!;
        const output =
          sub.result?.summary ??
          `worker '${p.workerId}' finished (${sub.result?.status ?? 'done'})`;
        results.push({ type: 'tool_result', tool_use_id: p.toolUseId, content: output });
      }
      if (results.length > 0) {
        task.append({ role: 'user', content: results });
      }
      task.setSuspend([]);
      task.markStatus('running');
    }
  }

  /** A task may be ticked now: active, not suspended, and not already in flight. */
  private isTickable(t: Task): boolean {
    return !this.inFlight.has(t.id) && (t.status === 'idle' || t.status === 'running');
  }

  /**
   * Ensure `task`'s owner holds (or just acquired) every required exclusive resource. Leases are held
   * for the task's whole run (across rounds) and released when it terminates. Returns false (blocking
   * the tick this round) if any required resource is held by another agent; partial acquisitions made
   * during this call are rolled back so nothing leaks.
   */
  private acquireRequired(task: Task): boolean {
    const owner = this.team.agents.get(task.ownerId);
    const required = owner?.profile.requires ?? [];
    if (required.length === 0) return true;
    let held = this.taskLeases.get(task.id);
    if (!held) {
      held = [];
      this.taskLeases.set(task.id, held);
    }
    const heldIds = new Set(held.map((l) => l.resource));
    const acquired: Lease[] = [];
    for (const resId of required) {
      if (heldIds.has(resId)) continue;
      const lease = this.team.resources.acquire(resId, task.ownerId);
      if (!lease) {
        for (const l of acquired) this.team.resources.release(l);
        return false;
      }
      acquired.push(lease);
      held.push(lease);
    }
    return true;
  }

  /** Release leases held by tasks that have reached a terminal state (worker done / error). */
  private releaseTerminated(): void {
    for (const [taskId, leases] of this.taskLeases) {
      const t = this.team.tasks.get(taskId);
      if (t && isTerminal(t)) {
        for (const l of leases) this.team.resources.release(l);
        this.taskLeases.delete(taskId);
      }
    }
  }

  /** The single active leader's currently-tickable task, if any. */
  private leaderTask(): Task | undefined {
    return [...this.team.tasks.values()].find(
      (t) => t.ownerId === this.team.leaderId && this.isTickable(t),
    );
  }
}
