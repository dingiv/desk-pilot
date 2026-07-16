# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

This is a pnpm monorepo (Node ≥20, pnpm v11). Dev uses `tsx` + `vitest`, which read TS directly — **no build step is needed to run or test**.

```bash
pnpm install              # link workspace packages (local only; no new external deps)
pnpm typecheck            # tsc --noEmit over the whole graph
pnpm test                 # vitest run (all tests, no API key needed)
pnpm test:watch           # vitest watch
pnpm build                # tsc --build (project references) → emits each package's dist/

# Run a subset of tests:
pnpm test -- test/scout-server.test.ts          # one file (pnpm forwards args to vitest)
pnpm exec vitest run -t "handshakes a new client"  # by test name
```

**Rust native driver layer** — a separate Cargo workspace at `crates/` (capture + input, feature-gated backends; supersedes `playground/nutjs`/`pyautogui`). It coexists with the pnpm monorepo and does **not** touch the TS side yet (napi→`NativeLayer` wiring is a future round). Toolchain is **not** preinstalled in the container — see `crates/README.md` for the one-time `rustup` + `apt` setup, then:
```bash
cargo build --workspace                  # core + stubs (always green, no toolchain deps in drivers)
cargo test --workspace                   # pure-logic tests (Frame/PNG, key/Button maps, trait defaults)
cargo build --workspace --all-features   # compile every real backend (evdev + pipewire + ashpd)
```
The `drivers` crate builds + tests with no native system libraries under default features; the PipeWire/uinput/libei backends are feature-gated modules within it (merged from three former standalone crates — `vrover-pipewire`/`-uinput`/`-libei`).

The root `package.json` holds only engineering commands (`pnpm typecheck` / `test` / `build` / `build:native`). Runnable entry points live in their own package and are invoked with `pnpm --filter @vrover/<name> <script>` (all run via `tsx`):
- Examples: `pnpm --filter @vrover/scout example` (Scout TCP server demo).
- Standalone apps (CLI-arg configured, see `apps/`): `pnpm --filter @vrover/visual-scout start`, `pnpm --filter @vrover/visual-rover-cli start:mock` / `start:desktop` (CLI one-shot), `pnpm --filter @vrover/visual-rover-web dev` (koa + React SPA, dev = Vite middleware). Pass args with `-- <args>` (e.g. `pnpm --filter @vrover/visual-scout start -- --port 9000`).
- DevTools UI (`apps/visual_scout_devtools`, a **Vue 3 + Vite client-rendered SPA**): `pnpm --filter @vrover/visual-scout-devtools dev` (Vite dev server, HMR) / `build` (`vue-tsc` + `vite build`) / `preview`. Point it at a scout devtools port with `SCOUT_DEVTOOLS_API=http://host:port pnpm --filter @vrover/visual-scout-devtools dev`.

API keys: the real LLM path needs an API key — set it in `vrover.conf` or via the corresponding env var (e.g. `ANTHROPIC_API_KEY`, `GLM_API_KEY`). The Scout server, the apps' boot, and all tests run **without** a key.

## Architecture

VRover is a visual GUI agent built around an **observe → think → act** loop where the model references UI elements by **Set-of-Mark number, never raw coordinates** (the tool executor resolves mark → element bounds → center → `Platform` primitive).

**Two-component split (`docs/decisions.md` D10) — the central architectural fact:**
- **VRover (the brain)** — `@vrover/agent` (the loop) + `@vrover/llm` (Anthropic adapter). Plans, calls the multimodal LLM, acts.
- **Visual Scout (the "fat tool")** — `@vrover/scout` (+ `-protocol`/`-client`) + `@vrover/platform` + `@vrover/som`. A standalone process that owns the real UI target.

The brain never touches the OS directly. It drives Scout over a **custom binary TCP protocol** via `RemotePlatform` (a thin adapter over the `@vrover/scout-client` SDK). The existing `runAgent`/SoM/tools are identical whether the platform is in-process (`MockPlatform`) or remote (`RemotePlatform`) — swapping the `Platform` is the only change.

**Scout protocol** (`@vrover/scout-protocol`): 12-byte big-endian header `[magic 'SC'][ver][type][id u32][len u32]` + JSON or raw-BLOB payload. A client sends `HAND_SHAKE` first; the server mints a per-connection **session**, each with its own `Platform` backend (capture + keyboard/mouse), and replies `HAND_SHAKE_ACK`. Screenshots cross the wire as raw PNG BLOBs (no base64). `UiElement`/`Bounds` are JSON-serializable and pass through as-is.

**Scout devtools service** (`@vrover/scout` `devtools.ts`; opt-in via `startScoutServer({devtoolsPort})` or `pnpm --filter @vrover/visual-scout start -- --devtools-port`): a second, **browser-friendly HTTP+SSE** port on the scout process. Browsers can't speak the raw TCP protocol, so this service is **in-process** — it builds plain `Request` objects and calls `Session.dispatch` (the *identical* path TCP clients take; captures decoded via `decodeCaptureBlob`), exposing REST (`/api/sessions`, `/:id/capture` (image/png), `/elements`, `/click|type|scroll|keypress`, `/config`) + SSE (`/:id/stream`) with permissive CORS. The web UI is `apps/visual_scout_devtools` — a **Vue 3 + Vite client-rendered SPA** (`pnpm --filter @vrover/visual-scout-devtools dev`) that calls only relative `/api/...` URLs; in dev/preview Vite proxies `/api` to the scout devtools port (`SCOUT_DEVTOOLS_API`), so the browser stays single-origin with the API. The TCP protocol stays untouched — devtools is an additive control plane sharing the session registry.

**Package dependency graph (acyclic):**
```
@vrover/scout-protocol (leaf)  ← { scout-client, platform }
platform                        ← { som, tools, scout, agent }
@vrover/agent                   ← consumes scout-client (the project's ONLY internal consumer of the standalone SDK, keeping it third-party-independent)
@vrover/llm (leaf)
apps/{visual_scout,visual_rover_cli,visual_rover_web,visual_scout_devtools}  ← consumers (apps, not libraries; the devtools app has no @vrover deps — pure HTTP client)
```

**Key seams (where to plug things in):**
- `Platform` (`@vrover/platform`) — adding a target (desktop/browser) = adding one implementation. `DesktopPlatform` is a reserved Rust (napi-rs) stub that throws until a `NativeLayer` is supplied.
- `CompleteFn` (`@vrover/llm`) — the single LLM exit point, dependency-injected into the loop. A new provider = a sibling module exporting a same-signature function. Tests inject a scripted fake LLM (see `scriptedComplete` in `test/scout-loop.test.ts`) — this is how the whole loop runs key-free.
- `GroundingSource` (`@vrover/platform`) — reserved seam for element detection (accessibility tree / CV+OCR / ML vision); today `Platform.getElements` stands in for it.

**Authoritative docs (in Chinese, "代码为准" = code is source of truth):** `docs/architecture.md` (as-built), `docs/design.md` (long-term vision: UI-graph walker), `docs/decisions.md` (open decisions D1–D11), `docs/scout-server.md` (Scout protocol/server), `docs/som.md`. The graph-walker/graph-map (`Walker`/`GraphMap` in `@vrover/scout`) are deliberate empty placeholders pending D1/D2.

## Design principles

- **Composition over inheritance.** The brain's pieces are *composed* — injected collaborators behind small interfaces — never assembled into a class hierarchy. An `Agent` composes a `Platform`, a `CompleteFn` (the single LLM exit point), a `DispatchFn` (tool executor), an optional `NativeParser`, tools, prompts, and a `MemoryManager`. Adding a target / provider / toolset / persistence backend = adding one implementation of the relevant interface, not a subclass. `Agent` is itself a **factory** for `Task`s: it holds collaborators + resolved config (+ memory) and *no* per-conversation state, so one Agent drives many independent tasks — the shape a multi-agent architecture needs. A `Task` owns one conversation's lifecycle (history/steps/status + `run`/`exec`/`goto`/`pause`) and composes that same shared brain.
- **Pure core, impure shell (side-effect separation).** Constructors are pure — no I/O. Config reading (`createAgent` calls `loadConfig`), filesystem I/O (`FileMemoryManager`), and all OS/network access (`Platform`, the LLM `complete`) live in factory functions and the injectable collaborators, never in the core. The core — `observe`/`act` (`step.ts`), `pruneForModel` (`context.ts`), the `Task` state machine — stays pure-ish and unit-testable with fakes. This is why the whole observe→think→act loop runs **key-free / network-free** in tests.

## Conventions & gotchas

- **ESM + NodeNext:** every relative import needs a `.js` extension (e.g. `import { runAgent } from './loop.js'`), even for `.ts` files. tsx/vitest resolve these to the TS source.
- **Source-resolving exports:** each `packages/*` has `exports["."] → ./src/index.ts`, so consumers import TS directly in dev. `pnpm build` emits `dist/` via composite project references. Do not change a package's `exports` to point at `dist` — that breaks the no-build dev loop.
- **`noUncheckedIndexedAccess` is ON** (`tsconfig.base.json`): `record[key]` is typed `T | undefined`. Narrow with a truthiness check or use `!`.
- **Composite project references:** every `packages/*` and `apps/*` `tsconfig.json` is `composite` with `references` to its direct `@vrover/*` deps, and must be listed in `tsconfig.build.json`. Both live one level under the root (`packages/<pkg>/`, `apps/<app>/`), so their `extends` and `references` paths use `../../`. **Exception:** `apps/visual_scout_devtools` is a Vite/Vue SPA — it is **excluded** from `tsconfig.json` (typecheck) and `tsconfig.build.json` (it needs DOM lib + bundler resolution). Build it with `vite build` and type-check it with `vue-tsc` (`pnpm --filter @vrover/visual-scout-devtools build` / `pnpm --filter @vrover/visual-scout-devtools typecheck`), not `tsc`.
- **pnpm forwards a literal `--`:** `pnpm <script> -- <args>` passes `--` *and* the args to the script, so CLI entry points that use `node:util` `parseArgs` must strip a leading `--` before parsing (see `forwardedArgs()` in the apps' `main.ts`).
- **`loadConfig()` (`@vrover/llm`) throws if `ANTHROPIC_API_KEY` is unset** and caches on first call. Call it lazily (only on the real-LLM path), never at server boot — the Scout server and the rover app must boot key-free.
- **Apps under `apps/*` are workspace packages**, not examples: they get their own `package.json` declaring `@vrover/*` deps and follow the same composite-tsconfig convention as `packages/*`. (`apps/visual_scout_devtools` is the Vite/Vue exception above — no `@vrover` deps, no composite tsconfig.)
