# @vrover/visual-rover-web

The **web frontend** for the VRover GUI agent (the "brain"): a React SPA served by a single
**koa** server that also owns the API. By default the agent loop runs self-contained against an
in-memory `MockPlatform` (no Visual Scout server, no `/dev/uinput`), but the target backend is
selectable at boot with `--platform` (`mock` | `remote` | `desktop`) — the same three the CLI
exposes.

## How it works

One koa process, one port, two jobs:

- **API** — `POST /api/run { task }` → `{ result, log }` runs one agent loop; `GET /api/health` is a liveness probe.
- **SPA** — in **dev** (`NODE_ENV != production`) koa mounts Vite's dev middleware (HMR); in **prod** it serves the built `web-dist/` statically with an SPA fallback.

The **server** dev/prod switch is `NODE_ENV`. The **client** (the React app) distinguishes the two
via Vite's `import.meta.env.DEV` / `.MODE` — Vite statically replaces these at build time, so only
client code can read them; `NODE_ENV` is the one switch both sides honour.

## Usage

```bash
pnpm --filter @vrover/visual-rover-web dev          # dev: koa + Vite middleware + HMR, http://127.0.0.1:8080
pnpm --filter @vrover/visual-rover-web build:web    # vite build → web-dist/
pnpm --filter @vrover/visual-rover-web start        # prod: koa serves web-dist/ + API
```

Flags: `--host`, `--port`, `--max-steps`, `--platform`, `--scout-host`, `--scout_port` (see `--help`).
The CLI frontend is `@vrover/visual-rover-cli`.

### Platform backends (`--platform`)

The web server drives **one** platform for its lifetime, picked at boot — the `Platform` is the
only thing that changes; the observe→think→act loop, SoM, and tools are identical.

| `--platform` | Target | Notes |
| --- | --- | --- |
| `mock` *(default)* | in-memory `MockPlatform` | Boots key-free, no OS access — the demo path. |
| `remote` | Visual Scout server | `--scout-host`/`--scout-port` (else `vrover.conf` `scout.*`); a Scout server must be running. |
| `desktop` | native Rust seam (`DesktopNativeLayerAdapter`) | Real capture/input on a physical desktop; falls back to placeholders in a headless container. |

```bash
pnpm --filter @vrover/visual-rover-web dev -- --platform mock                       # default
pnpm --filter @vrover/visual-rover-web dev -- --platform remote --scout-port 9000   # drive a Scout server
pnpm --filter @vrover/visual-rover-web dev -- --platform desktop                    # drive the local desktop
```
