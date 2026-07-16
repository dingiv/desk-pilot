/**
 * model.ts — Model as a first-class object: identity + context window + modalities, with capabilities
 * mixed in via `Completes` / `Grounds` (design.md §5.2).
 *
 * Today the brain takes a bare `CompleteFn`; that is the *function* form of one capability. A Model
 * is the *object* form — an endpoint connection plus the capability interfaces it provides. A chat
 * model is `Model & Completes`; a grounding model (GUI-TARS-class) is `Model & Grounds`. Models are
 * held by the team and referenced by agents; one agent binds a primary model matching its specialty.
 *
 * Status: introduces the Model object. `createChatModel` wraps an existing `CompleteFn` (the §8
 * step-3 migration — callers move from passing a function to passing a Model); `createGroundingModel`
 * wraps a `GroundFn` for the pixel-grounding path.
 */
import type { CompleteFn } from '@vrover/llm';
import type { Completes, Grounds, Observation, PlatformAction } from './actions.js';

/** Modalities a model accepts. */
export type Modality = 'text' | 'image' | 'audio' | 'video';

/** A model endpoint: identity + context window + modalities. Capabilities come from `Completes`/`Grounds`. */
export interface Model {
  readonly id: string;
  readonly contextWindow: number;
  readonly modalities: readonly Modality[];
  /**
   * 详细描述了一个大模型后端的能力
   */
  readonly description: string;
}

/** A chat-completion model (DeepSeek / GLM class). */
export type ChatModel = Model & Completes;
/** A pixel-grounding model (GUI-TARS class) — emits actions directly from a screenshot + hint. */
export type GroundingModel = Model & Grounds;

export interface ChatModelDeps {
  readonly id: string;
  readonly complete: CompleteFn;
  readonly description?: string;
  readonly contextWindow?: number;
  readonly modalities?: readonly Modality[];
}

/** Wrap a `CompleteFn` as a `ChatModel` (`Model & Completes`). */
export function createChatModel(deps: ChatModelDeps): ChatModel {
  return {
    id: deps.id,
    contextWindow: deps.contextWindow ?? 128_000,
    modalities: deps.modalities ?? ['text'],
    description: deps.description ?? 'chat model',
    complete: deps.complete,
  };
}

/** A grounding function: screenshot + hint → pixel action. */
export type GroundFn = (obs: Observation, hint: string) => Promise<PlatformAction>;

export interface GroundingModelDeps {
  readonly id: string;
  readonly ground: GroundFn;
  readonly description?: string;
  readonly contextWindow?: number;
  readonly modalities?: readonly Modality[];
}

/** Wrap a `GroundFn` as a `GroundingModel` (`Model & Grounds`). */
export function createGroundingModel(deps: GroundingModelDeps): GroundingModel {
  return {
    id: deps.id,
    contextWindow: deps.contextWindow ?? 128_000,
    modalities: deps.modalities ?? ['text', 'image'],
    description: deps.description ?? 'grounding model',
    ground: deps.ground,
  };
}
