import { runAgent, createAgent, RemotePlatform } from '@vrover/agent';
import type { Agent, TaskResult } from '@vrover/agent';
import { complete as completeAnthropic } from '@vrover/llm';
import type { CompleteFn } from '@vrover/llm';
import { createProviderFromEnv } from '@vrover/providers';
import { MockPlatform } from '@vrover/platform';
import { DesktopPlatform, DesktopNativeLayerAdapter } from '@vrover/platform';
import type { Platform } from '@vrover/platform';
import { loadConfig as loadVroverConfig } from '@vrover/config';
import type { VroverConfig } from '@vrover/config';
import { createLogger } from '@vrover/logger';

/** Tee point for the agent loop's progress lines into the unified server logger. */
const webLogger = createLogger('web/agent');

/**
 * Selectable platform backends — the same three the CLI exposes.
 * `mock` is the default (in-memory, no OS access, boots key-free).
 */
export type PlatformName = 'mock' | 'remote' | 'desktop';

export const PLATFORM_NAMES: readonly PlatformName[] = ['mock', 'remote', 'desktop'];

/**
 * Build the {@link Platform} for a given backend name, mirroring the CLI's `pickPlatform`.
 * Swapping the target is the *only* change — `runAgent`/SoM/tools are identical regardless.
 *   mock     → in-memory {@link MockPlatform}
 *   remote   → a Visual Scout server (`host`/`port` from `vrover.conf`)
 *   desktop  → the reserved Rust seam (native capture/input via {@link DesktopNativeLayerAdapter})
 */
export function createPlatform(name: PlatformName, cfg: VroverConfig): Platform {
  switch (name) {
    case 'mock':
      return new MockPlatform();
    case 'remote':
      return new RemotePlatform(cfg.scout.host, cfg.scout.port);
    case 'desktop':
      return new DesktopPlatform(new DesktopNativeLayerAdapter());
  }
}

/**
 * The VRover web agent service — a self-contained observe→think→act loop with a visual
 * frontend. Wires the unmodified {@link runAgent} loop to an in-memory {@link MockPlatform}
 * (no Visual Scout server, no `/dev/uinput`, no API key to boot) and returns the result plus
 * the aggregated progress log.
 *
 * `complete` and `platform` are **injectable**, so tests drive the whole loop with a scripted
 * fake LLM and a richer mock platform. The default `complete` reads config lazily — a missing
 * API key surfaces as a clear error *when a task runs*, not when the server boots.
 */
export interface RunAgentTaskOptions {
  /** The user's natural-language goal. */
  task: string;
  /** Max agent steps; `runAgent`'s default (from config) when omitted. */
  maxSteps?: number;
  /**
   * LLM exit point. Defaults to the provider selected by `vrover.conf`'s `llm.provider`
   * (`glm`→GLM native, `anthropic`→Anthropic adapter). Injectable for tests.
   */
  complete?: CompleteFn;
  /** Target the loop drives. Defaults to the in-memory {@link MockPlatform}. */
  platform?: Platform;
}

/** Outcome of {@link runAgentTask}: the agent's result plus the aggregated progress lines. */
export interface RunAgentTaskOutcome {
  result: TaskResult;
  /** Every progress line emitted by the loop, in order. */
  log: string[];
}

export async function runAgentTask(opts: RunAgentTaskOptions): Promise<RunAgentTaskOutcome> {
  const log: string[] = [];
  const platform = opts.platform ?? new MockPlatform();
  const { complete, ...agentCfg } = resolveConfig({ complete: opts.complete });

  const result = await runAgent({
    platform,
    complete,
    task: opts.task,
    maxSteps: opts.maxSteps ?? agentCfg.maxSteps,
    contextWindow: agentCfg.contextWindow,
    keepScreenshots: agentCfg.keepScreenshots,
    captureTimeoutMs: agentCfg.captureTimeoutMs,
    debug: agentCfg.debug,
    log: (line: string) => {
      log.push(line);
      webLogger.debug(line); // tee into the unified server logger (visible at LOG_LEVEL=debug)
    },
  });

  return { result, log };
}

/**
 * Build the shared {@link Agent} the web service drives every task from. Resolves config + provider
 * **once** and returns a single Agent — the documented factory-for-tasks shape (`agent.createTask`)
 * — so the service's `execute`/`stream` create independent tasks without re-reading config or
 * re-wiring the loop per call. One Agent, many tasks.
 *
 * **Lazy by design:** provider construction (e.g. `createGlm`) throws without an API key, so the
 * service must NOT build this at boot — it creates the Agent on first task, keeping the server
 * key-free until a real run.
 */
export function createWebAgent(platform: Platform): Agent {
  const { complete, ...agentCfg } = resolveConfig({});
  return createAgent({
    platform,
    complete,
    contextWindow: agentCfg.contextWindow,
    keepScreenshots: agentCfg.keepScreenshots,
    captureTimeoutMs: agentCfg.captureTimeoutMs,
    debug: agentCfg.debug,
    maxSteps: agentCfg.maxSteps,
    log: (line: string) => webLogger.debug(line), // tee the brain's progress trace into the server logger
  });
}

/**
 * Resolve the LLM exit point + agent config from `vrover.conf`. The caller-supplied `complete`
 * wins over config (the test seam); omit it to use the configured provider.
 */
function resolveConfig(opts: { complete?: CompleteFn }): {
  complete: CompleteFn;
  contextWindow: number;
  keepScreenshots: number;
  captureTimeoutMs: number;
  debug: boolean;
  maxSteps: number;
} {
  const cfg = loadVroverConfig();
  return {
    complete: opts.complete ?? pickProvider(cfg.llm.provider),
    contextWindow: cfg.agent.contextWindow,
    keepScreenshots: cfg.agent.keepScreenshots,
    captureTimeoutMs: cfg.agent.captureTimeoutMs,
    debug: cfg.agent.debug,
    maxSteps: cfg.agent.maxSteps,
  };
}

function pickProvider(name: string): CompleteFn {
  switch (name.toLowerCase()) {
    case 'anthropic':
      return completeAnthropic;
    case 'glm':
    case 'openai':
    case 'vllm':
    case 'custom':
      return createProviderFromEnv();
    default:
      return completeAnthropic;
  }
}
