/**
 * Per-session graph walker — **placeholder**.
 *
 * Per `docs/decisions.md` D10: the walker is session-level state — "which node
 * am I on, the traversal stack, the current UI state" — one per connection. A
 * real walker maintains the navigation stack that backs `go_back`, and walks
 * known edges without involving the multimodal brain.
 *
 * That logic depends on the still-open decisions D1 (node identity / matching)
 * and D2 (DSL), so this class is intentionally empty for now. Each {@link Session}
 * holds one so the session/walker boundary is visible in code today; the
 * navigation methods land later without reshaping the session.
 *
 * @see {@link GraphMap} for the shared, server-level counterpart.
 */
export class Walker {
  // Reserved: currentNodeId?, traversalStack: string[], current UI state, …
}
