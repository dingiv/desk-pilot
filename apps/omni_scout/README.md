# @vrover/visual-scout

The standalone **Visual Scout server** — runs the UI-operation + grounding service (`@vrover/scout`)
as its own process. Each connecting client handshakes and gets an isolated session with its own
backend terminal (a `Platform` = screen capture + keyboard/mouse). Needs **no API key**: it only
exposes UI operations over the custom TCP protocol. See [`docs/scout-server.md`](../../docs/scout-server.md).

## Run

```bash
pnpm --filter @vrover/visual-scout start                                  # defaults: multi-screen backend on 127.0.0.1:7878
pnpm --filter @vrover/visual-scout start -- --host 0.0.0.0 --port 9000    # bind publicly on 9000
pnpm --filter @vrover/visual-scout start -- --backend mock                # single-screen mock backend
pnpm --filter @vrover/visual-scout start -- --help                        # usage
```

### Options

| Flag | Default | Notes |
|---|---|---|
| `--host <host>` | `$SCOUT_HOST` or `127.0.0.1` | Bind host |
| `--port <port>` | `$SCOUT_PORT` or `7878` | Bind port; `0` = OS-assigned |
| `--backend <name>` | `multi-screen` | `multi-screen` \| `mock` \| `desktop` |
| `--devtools-port <port>` | (off) | Expose the browser devtools HTTP/SSE service; `0` = OS-assigned |
| `--devtools-host <host>` | `127.0.0.1` | Devtools bind host |
| `-h, --help` | | Show usage |

### DevTools

Add `--devtools-port` to also expose a browser-friendly devtools service (HTTP/SSE) that reuses the
same sessions as the TCP server — render screenshots, drive actions, configure state. Then run the
web UI:

```bash
pnpm --filter @vrover/visual-scout start -- --devtools-port 7881
pnpm --filter @vrover/visual-scout-devtools dev   # serves the UI; see apps/visual_scout_devtools
```

### Backends

- `multi-screen` (default) — in-memory two-screen app (login → home).
- `mock` — single-screen in-memory mock.
- `desktop` — reserved Rust seam; fails fast until the napi-rs native module is built.

A client may also hint a backend in its handshake (`backend` field); when it names a known
backend it overrides the CLI default for that session.
