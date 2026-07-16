/**
 * visual-rover-cli — the VRover GUI agent CLI (the "brain", one-shot front end).
 *
 *   pnpm --filter @vrover/visual-rover-cli start                       # interactive prompt, mock platform
 *   pnpm --filter @vrover/visual-rover-cli start -- --task "log in"    # one-shot, mock platform
 *   pnpm --filter @vrover/visual-rover-cli start -- --platform mock --provider glm --task "log in"
 *   pnpm --filter @vrover/visual-rover-cli start:desktop               # = start --platform desktop
 *
 * Config priority: defaults < /etc/vrover.conf < ~/.vrover/vrover.conf <
 *                  ./vrover.conf < env vars < CLI args
 */
import { parseArgs } from 'node:util';
import type { VroverConfig } from '@vrover/config';
import { runCli } from './cli.js';

const USAGE = `\
visual-rover-cli — VRover GUI agent (brain), one-shot CLI

Usage:
  visual-rover-cli [options]

Options:
  --platform <p>        mock | remote | desktop (default: mock)
  --scout-host <host>   Scout server host (remote platform; default: from config / env)
  --scout-port <port>   Scout server port (remote platform; default: from config / env)
  --provider <p>        anthropic | glm | openai | vllm | custom
  --task <text>         Task text (otherwise prompt / stdin)
  --max-steps <n>       Max agent steps (default: from config)
  --yolo-path <path>    icon_detect.onnx path for native OmniParser
  -h, --help            Show this help and exit

Config: vrover.conf (CWD → ~/.vrover → /etc) + env vars.`;

function main(): void {
  const { values } = parseArgs({
    options: {
      platform: { type: 'string' },
      'scout-host': { type: 'string' },
      'scout-port': { type: 'string' },
      provider: { type: 'string' },
      task: { type: 'string' },
      'max-steps': { type: 'string' },
      'yolo-path': { type: 'string' },
      help: { type: 'boolean', short: 'h' },
    },
    args: forwardedArgs(),
    strict: true,
  });

  if (values.help) {
    console.log(USAGE);
    return;
  }

  const platform = (values.platform ?? 'mock') as 'mock' | 'remote' | 'desktop';
  if (!['mock', 'remote', 'desktop'].includes(platform)) {
    console.error(`Invalid --platform "${platform}". Use 'mock', 'remote', or 'desktop'.`);
    process.exit(2);
  }

  // Build CLI overrides from flags (only set when the flag was provided).
  const cliOverrides = buildCliOverrides(values);

  void runCli({
    platform,
    task: values.task,
    overrides: cliOverrides,
  });
}

// ── CLI → config overrides ──────────────────────────────────────────────────

function buildCliOverrides(values: Record<string, unknown>): Partial<VroverConfig> {
  const overrides: Record<string, unknown> = {};

  const provider = values.provider as string | undefined;
  if (provider) setNested(overrides, ['llm', 'provider'], provider);

  const scoutHost = values['scout-host'] as string | undefined;
  if (scoutHost) setNested(overrides, ['scout', 'host'], scoutHost);

  const scoutPort = values['scout-port'] as string | undefined;
  if (scoutPort) setNested(overrides, ['scout', 'port'], parseUint(scoutPort, '--scout-port'));

  const maxSteps = values['max-steps'] as string | undefined;
  if (maxSteps) setNested(overrides, ['agent', 'maxSteps'], parseUint(maxSteps, '--max-steps'));

  const yoloPath = values['yolo-path'] as string | undefined;
  if (yoloPath) setNested(overrides, ['agent', 'yoloPath'], yoloPath);

  return overrides as Partial<VroverConfig>;
}

function setNested(obj: Record<string, unknown>, path: string[], value: unknown) {
  let cur = obj;
  for (let i = 0; i < path.length - 1; i++) {
    const k = path[i]!;
    if (!cur[k]) cur[k] = {};
    cur = cur[k] as Record<string, unknown>;
  }
  cur[path[path.length - 1]!] = value;
}

// ── helpers ────────────────────────────────────────────────────────────────

function parseUint(raw: string, flag: string): number {
  const n = Number(raw);
  if (!Number.isInteger(n) || n <= 0) {
    console.error(`Invalid ${flag} "${raw}" — expected a positive integer.`);
    process.exit(2);
  }
  return n;
}

function forwardedArgs(): string[] {
  const args = process.argv.slice(2);
  // pnpm forwards a literal `--` separator before the user args.
  // Remove exactly one `--` so parseArgs sees everything as options.
  const sepIdx = args.indexOf('--');
  if (sepIdx >= 0) {
    return [...args.slice(0, sepIdx), ...args.slice(sepIdx + 1)];
  }
  return args;
}

main();
