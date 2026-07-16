#!/usr/bin/env tsx
/**
 * Example: AgentTeam — Leader + CoderWorker + Bash tool + deepseek-v4-pro
 * Task: read @open-apa source code and summarize its architecture.
 *
 * Usage:
 *   DEEPSEEK_API_KEY=sk-... pnpm --filter @vrover/agent example
 *
 * Model wiring comes from @vrover/llm (`deepseekComplete`, config-driven via `llm.deepseek`
 * in vrover.conf / DEEPSEEK_* env vars); this file holds only the demo logic.
 */

import {
  createAgentTeam,
  createLeaderAgent,
  createChatModel,
  createAgent,
} from '@vrover/agent';
import { MockPlatform } from '@vrover/platform';
import { deepseekComplete, loadConfig } from '@vrover/llm';
import type { ToolDef, ContentBlock, CompleteFn } from '@vrover/llm';
import type {
  TeamAgent,
  TickOutcome,
  AgentProfile,
  Task,
} from '@vrover/agent';
import { execSync } from 'node:child_process';

// ═══════════════════════════════════════════════════════════════════════════
// 1. Deepseek model — adapter from @vrover/llm (config-driven via llm.deepseek)
// ═══════════════════════════════════════════════════════════════════════════

const deepseekCfg = loadConfig().deepseek;
const model = createChatModel({
  id: deepseekCfg.model,
  complete: deepseekComplete,
  description: 'deepseek via anthropic-compatible endpoint',
  contextWindow: 128_000,
});

// ═══════════════════════════════════════════════════════════════════════════
// 2. Bash tool — shared resource (exclusive: false)
// ═══════════════════════════════════════════════════════════════════════════

const BASH_TOOL: ToolDef = {
  name: 'bash',
  description:
    'Execute a shell command in the workspace. ' +
    'Use to list directories (ls), read files (cat), search code (grep), etc. ' +
    'Output is returned as plain text. Avoid destructive commands.',
  input_schema: {
    type: 'object',
    properties: {
      command: { type: 'string', description: 'Shell command to run' },
    },
    required: ['command'],
  },
};

const TARGET_DIR = '/workspaces/gui_agent/open-apa';

function runBash(command: string): string {
  const t0 = Date.now();
  try {
    const out = execSync(command, {
      encoding: 'utf8',
      timeout: 30_000,
      maxBuffer: 2 * 1024 * 1024,
      cwd: TARGET_DIR,
    }).trim();
    console.log(`    🔧 bash(${Date.now() - t0}ms): $ ${command}\n${out.slice(0, 240)}`);
    return out || '(empty stdout)';
  } catch (e: any) {
    const msg = `EXIT ${e.status ?? 1}\n${e.stdout ?? ''}\n${e.stderr ?? e.message}`;
    console.log(`    🔧 bash(${Date.now() - t0}ms): $ ${command}  → ${msg.slice(0, 120)}`);
    return msg;
  }
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. CoderWorker — TeamAgent that explores code via bash + LLM
// ═══════════════════════════════════════════════════════════════════════════

const FINISH_TOOL: ToolDef = {
  name: 'finish',
  description: 'Call when done exploring. Summarize your architectural findings.',
  input_schema: {
    type: 'object',
    properties: { summary: { type: 'string', description: 'Architecture summary' } },
    required: ['summary'],
  },
};

const CODER_SYSTEM = [
  'You are a code-reading specialist. Your job: explore a codebase and report its architecture.',
  '',
  'Workflow:',
  '1. Start with `ls` to see the top-level structure.',
  '2. Read key files with `cat` or `head`.',
  '3. Use `grep` to trace imports / class definitions / entry points.',
  '4. Build a mental model of the module graph, then call `finish` with a concise summary.',
  '',
  'Rules:',
  '- Use `bash` for ALL file-system access (ls, cat, grep, find, wc, etc.).',
  '- Be thorough but efficient — prefer reading entry points and key modules over exhaustiveness.',
  '- The summary should cover: purpose, main modules, how they connect, key abstractions.',
  '- Write the summary in Chinese, ≤400 chars.',
].join('\n');

interface CoderWorkerDeps {
  profile: AgentProfile;
  complete: CompleteFn;
  maxSteps?: number;
}

function createCoderWorker(deps: CoderWorkerDeps): TeamAgent {
  // A core Agent is held only as a Task factory — its loop is never driven.
  const core = createAgent({
    platform: new MockPlatform(),
    complete: deps.complete,
    tools: [BASH_TOOL, FINISH_TOOL],
    systemPrompt: CODER_SYSTEM,
    maxSteps: deps.maxSteps ?? 30,
  });

  const tick = async (task: Task): Promise<TickOutcome> => {
    const resp = await deps.complete({
      system: CODER_SYSTEM,
      messages: [...task.history],
      tools: [FINISH_TOOL, BASH_TOOL],
    });

    const raw: ContentBlock[] =
      resp.raw.length > 0 ? resp.raw : [{ type: 'text', text: resp.text ?? '' }];
    task.append({ role: 'assistant', content: raw });

    if (resp.toolUses.length === 0) {
      task.append({
        role: 'user',
        content: [
          { type: 'text', text: 'Use `bash` to explore files, or `finish` when done.' },
        ],
      });
      return { kind: 'progress' };
    }

    // Collect all tool_results into ONE user message (deepseek requires: each
    // assistant tool_use must have a matching tool_result in the very next message).
    const toolResults: ContentBlock[] = [];
    for (const tu of resp.toolUses) {
      if (tu.name === 'bash') {
        const input = tu.input as { command: string };
        const output = runBash(input.command);
        toolResults.push({ type: 'tool_result', tool_use_id: tu.id, content: output });
      } else if (tu.name === 'finish') {
        const summary = typeof tu.input.summary === 'string' ? tu.input.summary : undefined;
        return { kind: 'done', summary };
      }
    }
    if (toolResults.length > 0) {
      task.append({ role: 'user', content: toolResults });
    }
    return { kind: 'progress' };
  };

  return {
    profile: deps.profile,
    createTask: (goal, opts) => core.createTask(goal, opts),
    tick,
  };
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Wiring: profiles → agents → team → task → run
// ═══════════════════════════════════════════════════════════════════════════

async function main() {
  // ── Profiles ──
  const leaderProfile: AgentProfile = {
    id: 'leader',
    role: 'leader',
    specialties: ['planning', 'code-review'],
    bio: '任务规划者：分析用户需求，分解为代码阅读子任务，分派给 coder worker 执行。',
  };

  const coderProfile: AgentProfile = {
    id: 'coder',
    role: 'worker',
    specialties: ['coding', 'code-analysis', 'bash'],
    bio: '代码分析者：能用 bash 浏览代码仓库，阅读源码文件，总结模块架构。',
  };

  // ── Agents ──
  const roster = { workers: [coderProfile] };
  const leader = createLeaderAgent({
    profile: leaderProfile,
    complete: model.complete,
    roster,
    maxSteps: 50,
  });

  const coder = createCoderWorker({
    profile: coderProfile,
    complete: model.complete,
    maxSteps: 30,
  });

  // ── Team ──
  const team = createAgentTeam({
    leader,
    workers: [coder],
    resources: [
      {
        id: 'bash',
        capability: 'execute shell commands to read/cat/grep/find files',
        exclusive: false,
        kind: 'service',
      },
    ],
  });

  // ── Root Task ──
  const goal = [
    `阅读 ${TARGET_DIR} 的源码。`,
    '重点关注 agent_core/ 目录下的 Python 模块。',
    '理解：核心 agent 类有哪些、它们如何协作、模块依赖关系、关键入口点。',
    '用中文输出架构总结。',
  ].join(' ');

  const task = team.createTask(goal, { ownerId: leaderProfile.id });

  let stepCount = 0;
  task.on((ev) => {
    if (ev.type === 'step') {
      stepCount++;
      const s = ev.step;
      const labels = s?.actions.map((a) => a.name).join(', ') ?? '-';
      console.log(`\n── step ${s?.index}  elements=${s?.elements}  actions=[${labels}]`);
    }
    if (ev.type === 'log') console.log(`   ${ev.text}`);
    if (ev.type === 'done') {
      console.log(`\n✅ TEAM DONE\n   summary: ${ev.result?.summary ?? '(none)'}`);
    }
    if (ev.type === 'error') {
      console.log(`\n❌ TEAM ERROR: ${ev.result?.error}`);
    }
  });

  console.log('═'.repeat(64));
  console.log(`🚀 AgentTeam: leader="${leaderProfile.id}", workers=["${coderProfile.id}"]`);
  console.log(`🧠 Model: ${deepseekCfg.model} (via ${deepseekCfg.baseUrl})`);
  console.log(`📋 Goal: ${goal}\n`);

  const t0 = Date.now();
  const result = await team.run(task, { maxRounds: 100 });
  const elapsed = ((Date.now() - t0) / 1000).toFixed(1);

  console.log(`\n${'═'.repeat(64)}`);
  console.log(`🏁 ${result.status}  (${elapsed}s, ${result.steps.length} leader steps, ${stepCount} events)`);
  if (result.summary) console.log(`📝 ${result.summary}`);
  if (result.error) console.log(`💥 ${result.error}`);
}

main().catch((err) => {
  console.error('Fatal:', err);
  process.exit(1);
});
