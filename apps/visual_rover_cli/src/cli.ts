/**
 * CLI entry — wires config → provider → platform → native parser → agent loop.
 *
 *   pnpm --filter @vrover/visual-rover-cli start -- --task "click the login button"
 *   pnpm --filter @vrover/visual-rover-cli start -- --platform mock --provider glm --task "log in"
 *   pnpm --filter @vrover/visual-rover-cli start -- --platform remote --scout-port 9000
 *   pnpm --filter @vrover/visual-rover-cli start:desktop               # = start --platform desktop
 *   pnpm --filter @vrover/visual-rover-cli start -- --platform desktop --yolo-path weights/icon_detect.onnx
 */
import readline from 'node:readline/promises';
import { stdin, stdout } from 'node:process';
import { runAgent, RemotePlatform } from '@vrover/agent';
import type { TaskResult } from '@vrover/agent';
import { loadConfig } from '@vrover/config';
import type { VroverConfig } from '@vrover/config';
import { complete as completeAnthropic } from '@vrover/llm';
import type { CompleteFn } from '@vrover/llm';
import { createParser } from '@vrover/native';
import type { NativeParser } from '@vrover/native';
import type { Platform } from '@vrover/platform';
import { MockPlatform, DesktopPlatform, DesktopNativeLayerAdapter } from '@vrover/platform';
import { createProviderFromEnv } from '@vrover/providers';

// ── types ──────────────────────────────────────────────────────────────────

export interface CliOptions {
  platform: 'mock' | 'remote' | 'desktop';
  task?: string;
  /** CLI config overrides (merged on top of config files + env vars). */
  overrides?: Partial<VroverConfig>;
}

// ── entry ──────────────────────────────────────────────────────────────────

export async function runCli(opts: CliOptions): Promise<void> {
  // Merge: defaults < config files < env vars < CLI args
  const cfg = loadConfig(opts.overrides as Partial<VroverConfig> | undefined);
  const task = await resolveTask(opts.task);
  if (!task) {
    console.error('No task provided. Pass --task, type one at the prompt, or pipe it on stdin.');
    process.exitCode = 2;
    return;
  }

  console.log(`VRover agent (platform: ${opts.platform}, provider: ${cfg.llm.provider})`);
  console.log(`Task: ${task}\n`);

  // 1. pick the LLM
  const complete = pickProvider(cfg.llm.provider);

  // 2. pick the platform
  const platform = pickPlatform(opts.platform, cfg);

  // 3. optionally wire the native OmniParser (desktop or explicit --yolo-path)
  const wantNative = opts.platform === 'desktop' || !!opts.overrides?.agent?.yoloPath;
  const nativeParser = wantNative ? pickNativeParser(cfg.agent.yoloPath) : undefined;

  // 4. run the agent loop — agent config resolved here (the shell), not in createAgent
  let result: TaskResult;
  try {
    result = await runAgent({
      platform,
      complete,
      task,
      maxSteps: cfg.agent.maxSteps,
      contextWindow: cfg.agent.contextWindow,
      keepScreenshots: cfg.agent.keepScreenshots,
      captureTimeoutMs: cfg.agent.captureTimeoutMs,
      debug: cfg.agent.debug,
      nativeParser,
      log: (line) => console.log(line),
    });
  } catch (err) {
    console.error(`\n✗ ${errMsg(err)}`);
    process.exitCode = 1;
    return;
  }

  // 5. print outcome
  console.log('\n=== RESULT ===');
  console.log('status:', result.status, '| steps:', result.steps.length);
  if (result.summary) console.log('summary:', result.summary);
  if (result.error) console.log('error:', result.error);
  console.log(result.status === 'success' ? '\n✓ Task completed.' : '\n✗ Task did not complete.');
  process.exitCode = result.status === 'success' ? 0 : 1;
}

// ── provider ───────────────────────────────────────────────────────────────

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
      console.error(`Unknown provider "${name}". Using anthropic.`);
      return completeAnthropic;
  }
}

// ── platform ───────────────────────────────────────────────────────────────

function pickPlatform(platform: string, cfg: VroverConfig): Platform {
  switch (platform) {
    case 'mock':
      return new MockPlatform();
    case 'remote':
      return new RemotePlatform(cfg.scout.host, cfg.scout.port);
    case 'desktop':
      return new DesktopPlatform(new DesktopNativeLayerAdapter());
    default:
      throw new Error(`Unknown platform "${platform}".`);
  }
}

// ── native parser ──────────────────────────────────────────────────────────

function pickNativeParser(yoloPath?: string): NativeParser | undefined {
  if (!yoloPath) return undefined;
  try {
    return createParser({ yoloPath });
  } catch (err) {
    console.error(`Native OmniParser not available: build it with \`pnpm build:native\``);
    console.error(`  Also verify the model exists at ${yoloPath}`);
    return undefined;
  }
}

// ── task resolution ────────────────────────────────────────────────────────

async function resolveTask(task?: string): Promise<string | undefined> {
  const fromFlag = task?.trim();
  if (fromFlag) return fromFlag;
  if (stdin.isTTY) {
    const rl = readline.createInterface({ input: stdin, output: stdout });
    try {
      return (await rl.question('Describe the task: ')).trim() || undefined;
    } finally {
      rl.close();
    }
  }
  return (await readAllStdin()).trim() || undefined;
}

function readAllStdin(): Promise<string> {
  return new Promise((resolve) => {
    let data = '';
    stdin.setEncoding('utf8');
    stdin.on('data', (chunk: string) => {
      data += chunk;
    });
    stdin.on('end', () => resolve(data));
  });
}

// ── helpers ────────────────────────────────────────────────────────────────

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
