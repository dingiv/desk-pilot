/**
 * @vrover/agent — the VRover brain. The observe → think → act `runAgent` loop (drives any
 * `Platform` with any `CompleteFn`, with injectable tools + prompts), its types, the prompt
 * management registry, and `RemotePlatform` — the brain-side adapter that drives a remote Scout
 * server through the standalone `@vrover/scout-client` SDK.
 */
export { runAgent, createAgent, getAgentLogger } from './core.js';
export { FileMemoryManager } from './memory.js';
export { pruneForModel, turnBoundaries } from './context.js';
export type { PruneOptions } from './context.js';
export type {
  Agent,
  AgentDeps,
  AgentOptions,
  AgentStatus,
  MemoryManager,
  Task,
  TaskCapture,
  TaskEvent,
  TaskEventType,
  TaskListener,
  TaskResult,
  TaskSnapshot,
  TaskStatus,
  AgentStep,
  StepAction,
  DispatchFn,
} from './types.js';
export { RemotePlatform } from './remote.js';
export { PromptRegistry, prompts, render } from './prompts/index.js';
export type { PromptName, PromptVars } from './prompts/index.js';
// ── multi-agent execution layer (team loop · write lock · DeliverTask) ──
export {
  createAgentTeam,
  createLeaderAgent,
  createGUIAgent,
  createGroundingAgent,
  DELIVER_TASK_TOOL,
} from './team.js';
export {
  createDesktopTool,
  createResourceManager,
} from './resources.js';
export type {
  DesktopTool,
  Lease,
  Resource,
  ResourceManager,
  ResourceKind,
} from './resources.js';
export type {
  AgentProfile,
  AgentTeam,
  AgentTeamDeps,
  DelegateIntent,
  DelegateResolution,
  DeliverTaskInput,
  DeliverTaskResult,
  GUIAgent,
  GUIAgentDeps,
  GroundingAgentDeps,
  LeaderAgent,
  LeaderAgentDeps,
  TeamAgent,
  TeamLoop,
  TeamRoster,
  TickOutcome,
} from './team.js';
export type { TaskSuspendState } from './types.js';
// ── model + action interfaces (design.md §5.1/§5.2) ──
export { createChatModel, createGroundingModel } from './model.js';
export type { ChatModel, ChatModelDeps, GroundFn, GroundingModel, GroundingModelDeps, Modality, Model } from './model.js';
export { captureObservation, performPlatformAction } from './actions.js';
export type {
  Acts,
  ActionResult,
  Completes,
  Grounds,
  Observation,
  Observes,
  PlatformAction,
} from './actions.js';
