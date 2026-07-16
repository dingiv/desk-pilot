import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { DEFAULTS } from './defaults.js';
import type { VroverConfig } from './types.js';

// ── search paths (lowest → highest priority) ──────────────────────────────

/**
 * Walk up from `cwd` collecting every `vrover.conf`, root-first → cwd-last so the closest one
 * wins when layered (like tsconfig.json / package.json discovery). pnpm `--filter` runs each app
 * with `cwd` set to its package dir, so a config at the monorepo root is invisible to a plain
 * `./vrover.conf` lookup — walking up finds it regardless of which package launched the process.
 */
function projectConfigPaths(): string[] {
  const found: string[] = [];
  let dir = process.cwd();
  let prev = '';
  for (let i = 0; i < 16 && dir !== prev; i++) {
    found.push(resolve(dir, 'vrover.conf'));
    prev = dir;
    dir = dirname(dir);
  }
  found.reverse(); // root-first (lowest prio) → cwd (highest prio)
  return found;
}

function searchPaths(): string[] {
  return [
    resolve('/etc/vrover.conf'),
    resolve(homedir(), '.vrover/vrover.conf'),
    ...projectConfigPaths(),
  ];
}

// ── deep merge ─────────────────────────────────────────────────────────────

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** Deep-merge `source` into `target`. Mutates and returns `target`. */
function deepMerge(
  target: Record<string, unknown>,
  source: Record<string, unknown>,
): Record<string, unknown> {
  for (const key of Object.keys(source)) {
    const sv = source[key];
    const tv = target[key];
    if (isObject(sv) && isObject(tv)) {
      deepMerge(tv, sv);
    } else if (sv !== undefined) {
      target[key] = sv;
    }
  }
  return target;
}

// ── env-var overrides ──────────────────────────────────────────────────────

const ENV_MAP: [string[], string][] = [
  // llm.anthropic
  [['llm', 'anthropic', 'apiKey'], 'ANTHROPIC_API_KEY'],
  [['llm', 'anthropic', 'model'], 'ANTHROPIC_MODEL'],
  [['llm', 'anthropic', 'effort'], 'ANTHROPIC_EFFORT'],
  [['llm', 'anthropic', 'maxTokens'], 'ANTHROPIC_MAX_TOKENS'],
  // llm.glm
  [['llm', 'glm', 'apiKey'], 'GLM_API_KEY'],
  [['llm', 'glm', 'baseUrl'], 'GLM_BASE_URL'],
  [['llm', 'glm', 'visionModel'], 'GLM_VISION_MODEL'],
  // llm.openai
  [['llm', 'openai', 'apiKey'], 'OPENAI_API_KEY'],
  [['llm', 'openai', 'baseUrl'], 'OPENAI_BASE_URL'],
  [['llm', 'openai', 'model'], 'OPENAI_MODEL'],
  // llm.vllm
  [['llm', 'vllm', 'baseUrl'], 'VLLM_BASE_URL'],
  [['llm', 'vllm', 'apiKey'], 'VLLM_API_KEY'],
  [['llm', 'vllm', 'model'], 'VLLM_MODEL'],
  // llm.custom
  [['llm', 'custom', 'baseUrl'], 'LLM_BASE_URL'],
  [['llm', 'custom', 'apiKey'], 'LLM_API_KEY'],
  [['llm', 'custom', 'model'], 'LLM_MODEL'],
  // llm.deepseek
  [['llm', 'deepseek', 'apiKey'], 'DEEPSEEK_API_KEY'],
  [['llm', 'deepseek', 'baseUrl'], 'DEEPSEEK_BASE_URL'],
  [['llm', 'deepseek', 'model'], 'DEEPSEEK_MODEL'],
  // provider selection
  [['llm', 'provider'], 'LLM_PROVIDER'],
  // scout
  [['scout', 'host'], 'SCOUT_HOST'],
  [['scout', 'port'], 'SCOUT_PORT'],
  // agent
  [['agent', 'maxSteps'], 'MAX_STEPS'],
  [['agent', 'yoloPath'], 'YOLO_PATH'],
  [['agent', 'boxThreshold'], 'BOX_THRESHOLD'],
  [['agent', 'iouThreshold'], 'IOU_THRESHOLD'],
  [['agent', 'debug'], 'AGENT_DEBUG'],
  [['agent', 'captureTimeoutMs'], 'CAPTURE_TIMEOUT_MS'],
  [['agent', 'keepScreenshots'], 'KEEP_SCREENSHOTS'],
  [['agent', 'contextWindow'], 'CONTEXT_WINDOW'],
];

function setNested(obj: Record<string, unknown>, path: string[], value: unknown) {
  let cur = obj;
  for (let i = 0; i < path.length - 1; i++) {
    const k = path[i]!;
    if (!isObject(cur[k])) cur[k] = {};
    cur = cur[k] as Record<string, unknown>;
  }
  const last = path[path.length - 1]!;
  cur[last] = value;
}

function applyEnvOverrides(config: Record<string, unknown>): void {
  for (const [path, envName] of ENV_MAP) {
    const raw = process.env[envName];
    if (raw === undefined) continue;
    // Coerce numbers
    const last = path[path.length - 1]!;
    const isNum =
      last === 'port' ||
      last === 'maxTokens' ||
      last === 'maxSteps' ||
      last === 'boxThreshold' ||
      last === 'iouThreshold' ||
      last === 'captureTimeoutMs' ||
      last === 'keepScreenshots' ||
      last === 'contextWindow';
    setNested(config, path, isNum ? Number(raw) : raw);
  }
}

// ── public API ─────────────────────────────────────────────────────────────

/**
 * Load configuration from the layered `vrover.conf` search path, then
 * overlay env vars, then apply programmatic `overrides`.
 *
 * Priority: defaults < /etc/vrover.conf < ~/.vrover/vrover.conf <
 *           ancestor `vrover.conf` (nearest wins) < env vars < overrides
 */
export function loadConfig(overrides?: Partial<VroverConfig>): VroverConfig {
  // Start with defaults (deep clone so we don't mutate the const)
  let merged: Record<string, unknown> = JSON.parse(JSON.stringify(DEFAULTS));

  // Layer config files (low → high priority)
  for (const path of searchPaths()) {
    let raw: string;
    try {
      raw = readFileSync(path, 'utf-8');
    } catch (err) {
      // ENOENT is fine — the file just doesn't exist at this path
      if ((err as NodeJS.ErrnoException).code !== 'ENOENT') {
        throw new Error(
          `Failed to read config at ${path}: ${(err as Error).message}`,
        );
      }
      continue;
    }
    // Skip empty files (e.g. a placeholder vrover.conf created by touch)
    const trimmed = raw.trim();
    if (trimmed.length === 0) continue;
    try {
      const parsed = JSON.parse(trimmed) as Record<string, unknown>;
      merged = deepMerge(merged, parsed);
    } catch (err) {
      throw new Error(
        `Invalid JSON in config at ${path}: ${(err as Error).message}`,
      );
    }
  }

  // Env vars override config files
  applyEnvOverrides(merged);

  // Programmatic overrides are highest priority
  if (overrides) {
    merged = deepMerge(merged, overrides as unknown as Record<string, unknown>);
  }

  return merged as unknown as VroverConfig;
}
