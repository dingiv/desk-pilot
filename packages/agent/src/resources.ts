/**
 * resources.ts — the team resource / lease layer: shared service tools vs exclusive desktop.
 *
 * The key reframe (see `docs/design.md` §5.5): **`Platform` is an internal resource of a tool**. A
 * desktop resource owns a real `Platform` and exposes it as a `DesktopTool` (which implements
 * `Platform`) — agents inject the tool, never the raw platform, exactly like a web-search tool owns
 * an HTTP client internally. `acquire`/`release` govern *who may run* (the lease holder's task is the
 * one the team loop ticks); exclusivity is a scheduling concern, enforced by the team loop refusing
 * to tick a non-holder's task — not by a lock inside the tool.
 *
 * Status: first revision. The lease primitives + `DesktopTool` are real and tested; `select()` is a
 * naive substring match (real relevance ranking deferred); MCP / skills registration and `vrover.conf`
 * tool-attribute wiring are deferred (the `kind` tags reserve the seam).
 */
import type { Platform } from '@vrover/platform';

/** Resource category. The literal union documents the known kinds; `(string & {})` keeps it open. */
export type ResourceKind = 'service' | 'desktop' | 'image' | 'mcp' | 'skill' | (string & {});

/** A pool entry: a capability description (for PromptBuilder / routing) + an access semantic. */
export interface Resource {
  readonly id: string;
  /** Human / model-readable: what this resource does. */
  readonly capability: string;
  /** Exclusive resources are held by ≤1 agent at a time (a desktop); shared ones grant freely. */
  readonly exclusive: boolean;
  readonly kind: ResourceKind;
}

/** A held resource. For exclusive resources, `holder` is the sole agent whose task may advance. */
export interface Lease {
  readonly resource: string;
  readonly holder: string; // agentId
}

/** Owns the resource pool + lease bookkeeping. */
export interface ResourceManager {
  register(resource: Resource): void;
  /** Grant a lease, or return `null` if the resource is exclusive and held by another agent. */
  acquire(resId: string, holder: string): Lease | null;
  release(lease: Lease): void;
  /** Current exclusive holder, if any. */
  holder(resId: string): string | undefined;
  /** Resources whose capability/id matches `need` (naive substring; relevance ranking deferred). */
  select(need: string): Resource[];
}

/** In-memory resource manager. */
export function createResourceManager(initial: readonly Resource[] = []): ResourceManager {
  const resources = new Map<string, Resource>();
  const holderOf = new Map<string, string>(); // resId → holder agentId (exclusive only)
  for (const r of initial) resources.set(r.id, r);

  return {
    register(r) {
      resources.set(r.id, r);
    },
    acquire(resId, holder) {
      const r = resources.get(resId);
      if (!r) throw new Error(`acquire: unknown resource '${resId}'`);
      if (!r.exclusive) return { resource: resId, holder }; // shared: always grant
      const cur = holderOf.get(resId);
      if (cur === undefined) {
        holderOf.set(resId, holder);
        return { resource: resId, holder };
      }
      if (cur === holder) return { resource: resId, holder }; // already held by same agent (idempotent)
      return null; // held by another agent
    },
    release(lease) {
      const cur = holderOf.get(lease.resource);
      if (cur === lease.holder) holderOf.delete(lease.resource);
    },
    holder(resId) {
      return holderOf.get(resId);
    },
    select(need) {
      const n = need.toLowerCase();
      return [...resources.values()].filter(
        (r) => r.capability.toLowerCase().includes(n) || r.id.toLowerCase().includes(n),
      );
    },
  };
}

/**
 * A desktop tool: owns a real {@link Platform} internally and re-exposes it as the tool surface (it
 * IS a `Platform`). Agents inject this — never the raw platform. Exclusivity is enforced by the team
 * loop (it won't tick a non-holder's task), so the tool itself is a pure delegating wrapper.
 */
export interface DesktopTool extends Platform {
  readonly resource: Resource;
}

/** Wrap a real platform as an exclusive desktop tool carrying its own resource descriptor. */
export function createDesktopTool(
  id: string,
  real: Platform,
  capability = 'exclusive desktop session (capture + input)',
): DesktopTool {
  const resource: Resource = { id, capability, exclusive: true, kind: 'desktop' };
  return {
    resource,
    captureScreen: () => real.captureScreen(),
    getElements: () => real.getElements(),
    performClick: (x, y) => real.performClick(x, y),
    performType: (text) => real.performType(text),
    performScroll: (x, y, direction) => real.performScroll(x, y, direction),
    performKeypress: (keys) => real.performKeypress(keys),
  };
}
