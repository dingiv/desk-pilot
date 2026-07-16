# @vrover/visual-scout-devtools

The **browser DevTools UI** for the Visual Scout server — a **Vue 3 + Vite client-rendered SPA**
(no SSR). It drives scout's in-process **devtools service** (HTTP/SSE): inspect sessions, render the
screenshot with a Set-of-Mark overlay, drive actions, and configure server state. Browsers can't
speak scout's raw TCP protocol, so the scout process bridges:
**browser ⇄ HTTP/SSE ⇄ scout devtools service ⇄ `Session.dispatch`**.

The SPA calls only relative `/api/...` URLs; in dev (`vite`) and preview (`vite preview`) Vite
**proxies `/api`** to the scout devtools port — single origin, no CORS, SSE streams cleanly.

## Run

```bash
# 1. start scout with the devtools service on an extra port
pnpm --filter @vrover/visual-scout start -- --devtools-port 7881

# 2. run the UI (Vite dev server, HMR) — open the printed http://localhost:9090
pnpm --filter @vrover/visual-scout-devtools dev

# point at a non-default scout devtools port:
SCOUT_DEVTOOLS_API=http://127.0.0.1:7881 pnpm --filter @vrover/visual-scout-devtools dev
```

No API key is needed — the devtools drives scout backends directly.

### Scripts

| Script | What it does |
|---|---|
| `pnpm --filter @vrover/visual-scout-devtools dev` | Vite dev server (HMR) on :9090, proxy `/api` → scout devtools. |
| `pnpm --filter @vrover/visual-scout-devtools build` | `vue-tsc --noEmit && vite build` → `dist/` (type-check + production bundle). |
| `pnpm --filter @vrover/visual-scout-devtools preview` | Serve the built `dist/` (`vite preview`), same `/api` proxy. |

> This app is **excluded** from the repo's `tsc` typecheck/build graph (it needs DOM lib + bundler
> resolution). Type-check it with `vue-tsc` and build it with `vite` — both via the scripts above.

### What the UI does

- **Sessions** — list (incl. live TCP-client sessions, so you can watch the brain drive a backend), create (pick a backend), select, delete.
- **Viewport** — the captured screenshot on a `<canvas>` with numbered SoM boxes; click the canvas to send a raw-coordinate click, or click an element in the list to click its center.
- **Actions** — type / keypress / scroll / refresh.
- **Live** — an `EventSource` on `/api/sessions/:id/stream` streams frames in real time.
- **Config** — GET/PUT `/api/config` (SSE capture interval, active session) — the "configure server state" surface.

### Layout

```
index.html               Vite entry
vite.config.ts           Vue plugin + server/preview proxy (SCOUT_DEVTOOLS_API)
src/
  main.ts                createApp(App).mount('#app')
  App.vue                3-column layout; provides the devtools store
  api.ts                 typed fetch + EventSource client (relative /api)
  types.ts               UiElement / SessionInfo / DevtoolsConfig / Frame
  composables/useDevtools.ts   reactive store (sessions, capture, live, config)
  components/{SessionPanel,Viewport,ActionBar,ConfigPanel}.vue
  style.css
```

### Scout devtools HTTP/SSE API (`/api`)

`GET /api/health` · `GET|POST /api/sessions` · `DELETE /api/sessions/:id` ·
`GET /api/sessions/:id/capture` (image/png) · `GET /api/sessions/:id/elements` ·
`POST /api/sessions/:id/{click|type|scroll|keypress}` · `GET /api/sessions/:id/stream` (SSE) ·
`GET|PUT /api/config`. The service lives in `packages/scout` (`devtools.ts`); CORS is enabled too.
