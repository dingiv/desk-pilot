/**
 * core.ts — the VRover brain. The stateful {@link Agent}/{@link Task} (one brain driving many
 * independent observe→think→act conversations) plus the thin one-shot {@link runAgent} wrapper.
 * A step is implemented once, here, composing the injected Platform / CompleteFn / dispatcher /
 * parser / tools / prompts.
 */
import { randomUUID } from 'node:crypto';
import type { LLMResponse, Message, ToolDef } from '@vrover/llm';
import type { NativeParser } from '@vrover/native';
import type { Platform } from '@vrover/platform';
import { formatTable } from '@vrover/som';
import { TOOL_DEFS, dispatch as defaultDispatch } from '@vrover/tools';
import { pruneForModel, turnBoundaries } from './context.js';
import { createChatModel } from './model.js';
import type { ChatModel } from './model.js';
import { prompts } from './prompts/index.js';
import { act, errMsg, observe } from './step.js';
import { createLogger, type Logger } from '@vrover/logger';
import type {
  Agent,
  AgentDeps,
  AgentOptions,
  AgentStatus,
  AgentStep,
  DispatchFn,
  MemoryManager,
  Task,
  TaskEvent,
  TaskListener,
  TaskResult,
  TaskSnapshot,
  TaskSuspendState,
} from './types.js';

// The brain's logger is created lazily on first use and cached for the process — the same
// lifecycle as `loadConfig()` (`@vrover/config`): deferred from module init, then one shared object.
let _agentLogger: Logger | undefined;

/**
 * Lazily-created, cached logger for the brain's progress trace (the default {@link AgentDeps.log}
 * sink). Created on first call rather than at module load, mirroring {@link loadConfig}.
 */
export function getAgentLogger(): Logger {
  return (_agentLogger ??= createLogger('agent'));
}

/**
 * Resolved collaborators + config + memory — the shared "brain". `createAgent` builds it (the one
 * place config is read); both {@link AgentImpl} and {@link TaskImpl} hold a reference. This is the
 * composition seam: a `Task` composes a `Brain` (plus its own state), never a concrete back-ref to
 * `AgentImpl`.
 */
interface Brain {
  readonly platform: Platform;
  readonly model: ChatModel;
  readonly runTool: DispatchFn;
  readonly nativeParser: NativeParser | undefined;
  readonly tools: ToolDef[];
  readonly system: string;
  readonly log: (message: string) => void;
  readonly contextWindow: number;
  readonly keepScreenshots: number;
  readonly captureTimeoutMs: number;
  readonly debug: boolean;
  readonly singleStep: boolean;
  readonly maxStepsDefault: number;
  readonly memory: MemoryManager | undefined;
}

/**
 * Wire up an {@link Agent}. All config is resolved by the caller and passed via `deps` —
 * this factory is pure (no I/O, no `loadConfig`). Sensible hardcoded fallbacks match the
 * defaults in `@vrover/config` so tests work with zero config.
 */
export function createAgent(deps: AgentDeps): Agent {

  // TODO: 新增参数校验 deps

  const brain: Brain = {
    platform: deps.platform,
    model: resolveModel(deps),
    runTool: deps.dispatch ?? defaultDispatch,
    nativeParser: deps.nativeParser,
    tools: deps.tools ?? TOOL_DEFS,
    system: deps.systemPrompt ?? prompts.render('system'),
    // Default sink routes the brain's progress trace through the unified logger at `debug`, so it
    // is silent under the default `info` threshold (tests stay quiet) but lights up at
    // `LOG_LEVEL=debug`. Callers wanting a user-facing stream (e.g. the web UI) inject their own.
    log: deps.log ?? ((msg: string) => getAgentLogger().debug(msg)),
    contextWindow: deps.contextWindow ?? 4,
    keepScreenshots: deps.keepScreenshots ?? 2,
    captureTimeoutMs: deps.captureTimeoutMs ?? 30000,
    debug: deps.debug ?? false,
    singleStep: deps.singleStep ?? false,
    maxStepsDefault: deps.maxSteps ?? 15,
    memory: deps.memory,
  };
  return new AgentImpl(brain);
}

/**
 * Resolve the agent's primary chat model: an explicit `model` wins; otherwise a legacy `complete` is
 * wrapped into a `ChatModel` (§8 step-3 migration). At least one must be supplied.
 */
function resolveModel(deps: AgentDeps): ChatModel {
  if (deps.model) return deps.model;
  if (deps.complete) return createChatModel({ id: 'agent', complete: deps.complete });
  throw new Error('AgentDeps requires either `model` or `complete`');
}

/**
 * The observe → think → act loop, run to completion. A thin wrapper over the stateful {@link Agent}
 * — `createAgent(opts).run(task)` — so a step is implemented once (in this module's {@link AgentImpl}
 * / {@link TaskImpl}, which compose the injected Platform / CompleteFn / dispatcher / parser / tools
 * / prompts).
 *
 * Runs until the model calls `done`, until `maxSteps`, or until an LLM error. For step-by-step
 * control (single-step, pause, rewind via `goto`), use `createAgent()` directly.
 */
// TODO: runAgent --> runTask(agent: Agent, opts: TaskOptions)
export async function runAgent(opts: AgentOptions): Promise<TaskResult> {

  if (opts.singleStep) {
    throw new Error(
      'singleStep mode requires the createAgent + task.run() + task.step() API; runAgent is one-shot ' +
        'and has no step() handle (it would deadlock).',
    );
  }
  return createAgent(opts).run(opts.task, { maxSteps: opts.maxSteps });
}

/** A wired-up brain: factory + holder of the shared collaborators/config/memory. No per-task state. */
class AgentImpl implements Agent {
  constructor(private readonly brain: Brain) {}

  get memory(): MemoryManager | undefined {
    return this.brain.memory;
  }

  createTask(goal: string, opts?: { id?: string; ownerId?: string }): Task {
    const id = opts?.id ?? randomUUID();
    return new TaskImpl(this.brain, goal, id, opts?.ownerId ?? '');
  }

  async loadTask(id: string): Promise<Task | null> {
    if (!this.brain.memory) {
      throw new Error('Agent has no MemoryManager — cannot loadTask.');
    }
    const snap = await this.brain.memory.load(id);
    return snap ? TaskImpl.fromSnapshot(this.brain, snap) : null;
  }

  /** Convenience: create a task and run it to a stop. (`runAgent` uses this.) */
  run(goal: string, opts?: { maxSteps?: number }): Promise<TaskResult> {
    return this.createTask(goal).run(opts);
  }
}

/**
 * The lifecycle of one conversation task. Holds the history/steps/status; delegates observe/act to
 * `./step.js` and context pruning to `./context.js`. Created by an {@link AgentImpl} with a shared
 * {@link Brain}.
 */
class TaskImpl implements Task {
  private readonly brain: Brain;
  private readonly _id: string;
  private readonly _goal: string;
  private _history: Message[];
  private _steps: AgentStep[] = [];
  private _status: AgentStatus = 'idle';
  private _result: TaskResult | undefined;
  private _ownerId = '';
  private _suspendedOn: TaskSuspendState[] = [];
  private pauseRequested = false;
  private stepCounter = 0;
  private readonly listeners = new Set<TaskListener>();
  /** Single-step gate: `step()` resolves the pending between-iteration wait (or latches if early). */
  private stepResolve: (() => void) | null = null;
  private stepLatched = false;

  constructor(brain: Brain, goal: string, id: string, ownerId = '') {
    this.brain = brain;
    this._goal = goal;
    this._id = id;
    this._ownerId = ownerId;
    this._history = [{ role: 'user', content: [{ type: 'text', text: goal }] }];
  }

  /** Restore a task from a persisted snapshot (turn boundaries re-derive from history on demand). */
  static fromSnapshot(brain: Brain, snap: TaskSnapshot): TaskImpl {
    const t = new TaskImpl(brain, snap.goal, snap.id, snap.ownerId ?? '');
    t._history = snap.history;
    t._steps = snap.steps;
    t._status = snap.status === 'running' ? 'idle' : snap.status; // a restored task is never mid-loop
    t._result = snap.result;
    t.stepCounter = snap.steps.length;
    t.pauseRequested = false;
    return t;
  }

  // ── accessors ─────────────────────────────────────────────────────────────
  get id(): string {
    return this._id;
  }
  get goal(): string {
    return this._goal;
  }
  get status(): AgentStatus {
    return this._status;
  }
  get history(): readonly Message[] {
    return this._history;
  }
  get steps(): readonly AgentStep[] {
    return this._steps;
  }
  get result(): TaskResult | undefined {
    return this._result;
  }
  get singleStep(): boolean {
    return this.brain.singleStep;
  }
  get ownerId(): string {
    return this._ownerId;
  }
  get suspendedOn(): readonly TaskSuspendState[] {
    return this._suspendedOn;
  }
  get runnable(): boolean {
    return (
      (this._status === 'idle' || this._status === 'running') && this._suspendedOn.length === 0
    );
  }

  // ── external-driver seam (team loop); inert for the standalone GUI loop ─────────────
  append(message: Message): void {
    this._history.push(message);
  }
  markStatus(status: AgentStatus): void {
    this._status = status;
  }
  setResult(result: TaskResult): void {
    this._result = result;
  }
  setSuspend(states: readonly TaskSuspendState[]): void {
    this._suspendedOn = [...states];
  }

  // ── auto-pilot ────────────────────────────────────────────────────────────
  async run(opts?: { maxSteps?: number }): Promise<TaskResult> {
    if (this._status === 'done' || this._status === 'error') {
      return this._result ?? { status: 'max_steps', steps: this._steps };
    }
    const maxSteps = opts?.maxSteps ?? this.brain.maxStepsDefault;
    this._status = 'running';
    this.pauseRequested = false;
    this.stepLatched = false; // reset any leftover single-step latch (e.g. resume after a pause)

    while (this._status === 'running' && !this.pauseRequested && this.stepCounter < maxSteps) {
      await this.exec();
      // Single-step debug: after one iteration, block until `step()` (the continue command) —
      // unless the task just reached a terminal state or was paused.
      if (
        this.brain.singleStep &&
        this._status === 'running' &&
        !this.pauseRequested &&
        this.stepCounter < maxSteps
      ) {
        this.emit({ type: 'log', text: `Single-step: step ${this.stepCounter} done — call step() to continue.` });
        await this.awaitStepContinue();
      }
    }

    // exec() sets _result/_status on done|error. Handle the two loop-exit reasons here:
    if (this._status === 'running') {
      if (this.pauseRequested) {
        this._status = 'paused';
        this._result = { status: 'paused', steps: this._steps };
        this.brain.log('Task paused.');
        this.emit({ type: 'paused', result: this._result });
      } else {
        this._status = 'done';
        this._result = { status: 'max_steps', steps: this._steps };
        const msg = `Reached max steps (${maxSteps}) without finishing.`;
        this.brain.log(msg);
        this.emit({ type: 'log', text: msg });
        this.emit({ type: 'done', result: this._result });
      }
    }
    return this._result!;
  }

  // ── one chat iteration ────────────────────────────────────────────────────
  async exec(opts?: { message?: string }): Promise<AgentStep | null> {
    if (this._status === 'done' || this._status === 'error') return null;
    const b = this.brain;
    const step = this.stepCounter + 1;
    const tStep = b.debug ? performance.now() : 0;

    // Wrapped log: routes to both the injected log sink AND every streaming listener.
    const log = (msg: string) => {
      b.log(msg);
      this.emit({ type: 'log', text: msg });
    };

    if (opts?.message) {
      this._history.push({ role: 'user', content: [{ type: 'text', text: opts.message }] });
    }

    // ── observe ─────────────────────────────────────────────────────────────
    const som = await observe(b.platform, b.nativeParser, b.captureTimeoutMs);
    this._history.push({
      role: 'user',
      content: [
        { type: 'image', mediaType: 'image/png', data: som.annotated.png },
        { type: 'text', text: prompts.render('step', { step, elementTable: formatTable(som.table) }) },
      ],
    });
    // Stream the SoM-annotated screenshot to listeners (e.g. the web UI's capture gallery) as soon
    // as it's grabbed — ahead of think/act, so each capture renders live. Built only when someone is
    // listening: the base64 encode is needless work otherwise (no-listener paths skip it entirely).
    if (this.listeners.size > 0) {
      this.emit({
        type: 'capture',
        capture: {
          step,
          dataUrl: `data:image/png;base64,${som.annotated.png.toString('base64')}`,
        },
      });
    }
    log(
      b.debug
        ? `Step ${step}: ${som.table.length} elements (observe ${(performance.now() - tStep).toFixed(0)}ms)`
        : `Step ${step}: ${som.table.length} elements visible.`,
    );

    // ── think ───────────────────────────────────────────────────────────────
    let resp: LLMResponse;
    try {
      resp = await b.model.complete({
        system: b.system,
        messages: pruneForModel(this._history, {
          contextWindow: b.contextWindow,
          keepScreenshots: b.keepScreenshots,
        }),
        tools: b.tools,
      });
    } catch (err) {
      const m = errMsg(err);
      log(`LLM error: ${m}`);
      this._status = 'error';
      this._result = { status: 'error', error: m, steps: this._steps };
      this.emit({ type: 'error', result: this._result });
      return null;
    }
    this.logModelOutput(log, resp);

    // ── act (or nudge if the model only talked) ─────────────────────────────
    if (resp.toolUses.length === 0) {
      this._history.push({ role: 'assistant', content: resp.raw });
      this._history.push({
        role: 'user',
        content: [{ type: 'text', text: prompts.render('nudge') }],
      });
      const stepRecord: AgentStep = { index: step, elements: som.table.length, actions: [] };
      this.commitStep(stepRecord);
      this.emit({ type: 'step', step: stepRecord });
      return stepRecord;
    }

    this._history.push({ role: 'assistant', content: resp.raw });
    const { actions, toolResults, finished, summary } = await act(
      resp.toolUses,
      som.table,
      b.platform,
      b.runTool,
      log,
    );
    this._history.push({ role: 'user', content: toolResults });
    const stepRecord: AgentStep = { index: step, elements: som.table.length, actions };
    this.commitStep(stepRecord);
    this.emit({ type: 'step', step: stepRecord });

    if (finished) {
      log(`Task complete${summary ? `: ${summary}` : ''}.`);
      this._status = 'done';
      this._result = { status: 'success', summary, steps: this._steps };
      this.emit({ type: 'done', result: this._result });
    }
    return stepRecord;
  }

  // ── rewind ─────────────────────────────────────────────────────────────────
  goto(step: number): void {
    const bounds = turnBoundaries(this._history);
    const n = Math.max(0, Math.min(step, this._steps.length));
    if (n === 0) {
      this._history = [{ role: 'user', content: [{ type: 'text', text: this._goal }] }];
    } else {
      this._history = this._history.slice(0, bounds[n - 1]!);
    }
    this._steps = this._steps.slice(0, n);
    this.stepCounter = n;
    this._status = 'idle';
    this._result = undefined;
    this.pauseRequested = false;
  }

  // ── interrupt ──────────────────────────────────────────────────────────────
  pause(): void {
    this.pauseRequested = true;
    this.releaseStepGate(); // unblock a pending single-step wait so the loop sees pauseRequested
  }

  /**
   * Single-step continue: release the between-iteration wait so the loop advances exactly one
   * iteration. Race-safe — if called before the wait is set up (e.g. mid-iteration), it latches and
   * the next wait returns immediately; redundant calls coalesce into one pending advance.
   */
  step(): void {
    if (this.stepResolve) {
      const r = this.stepResolve;
      this.stepResolve = null;
      r();
    } else {
      this.stepLatched = true;
    }
  }

  /** Wake a pending single-step wait (used by `pause()` so a waiting loop can exit). */
  private releaseStepGate(): void {
    if (this.stepResolve) {
      const r = this.stepResolve;
      this.stepResolve = null;
      r();
    }
  }

  /** In single-step mode, block after each iteration until `step()` (or `pause()`/terminal). */
  private async awaitStepContinue(): Promise<void> {
    if (this.stepLatched) {
      this.stepLatched = false;
      return;
    }
    await new Promise<void>((resolve) => {
      this.stepResolve = resolve;
    });
  }

  // ── persist ────────────────────────────────────────────────────────────────
  async save(): Promise<void> {
    if (!this.brain.memory) {
      throw new Error('Agent has no MemoryManager — cannot save task.');
    }
    await this.brain.memory.save({
      id: this._id,
      goal: this._goal,
      history: this._history,
      steps: this._steps,
      status: this._status,
      result: this._result,
      ownerId: this._ownerId,
    });
  }

  // ── streaming ─────────────────────────────────────────────────────────────
  on(listener: TaskListener): void {
    this.listeners.add(listener);
  }
  off(listener: TaskListener): void {
    this.listeners.delete(listener);
  }
  private emit(event: TaskEvent): void {
    for (const l of this.listeners) {
      try {
        l(event);
      } catch {
        // A listener must never break the loop.
      }
    }
  }

  // ── helpers ────────────────────────────────────────────────────────────────
  private commitStep(step: AgentStep): void {
    this._steps.push(step);
    this.stepCounter = step.index;
  }

  /** Debug box-drawing for the model's text + tool calls (mirrors the original loop). */
  private logModelOutput(log: (msg: string) => void, resp: LLMResponse): void {
    if (!this.brain.debug) {
      if (resp.text) log(`  model: ${resp.text.trim()}`);
      return;
    }
    if (resp.text) {
      log('  ┌─ model text ─────────────────────────────────');
      for (const line of resp.text.split('\n')) log(`  │ ${line}`);
    }
    if (resp.toolUses.length > 0) {
      const prefix = resp.text ? '  ├─' : '  ┌─';
      log(`${prefix} tool calls (${resp.toolUses.length}) ──────────────────────`);
      for (const tu of resp.toolUses) log(`  │  ${tu.name}(${JSON.stringify(tu.input)})`);
    }
    if (resp.text || resp.toolUses.length > 0) log('  └──────────────────────────────────────────');
  }
}
