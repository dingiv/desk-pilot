/**
 * Shared graph map — **placeholder**.
 *
 * Per `docs/decisions.md` D10: the graph map is server-level knowledge — a
 * persistent, cross-connection map of the app (nodes / edges / DSL), one per
 * application, shared by every session's {@link Walker}. Distinct from the
 * walker: the map is the knowledge, the walker is per-session navigation state.
 *
 * Building a real map needs node identity (D1) and the DSL (D2), which are still
 * undecided 🟡, so this class is intentionally empty. The server holds one
 * instance as shared state so the graph-map-vs-walker boundary is concrete today.
 */
export class GraphMap {
  // Reserved: nodes, edges, DSL persistence, lookups by signature, …
}
