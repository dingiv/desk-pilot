# `crates/` — VRover native layer (Rust)

A Cargo workspace living alongside the pnpm TS monorepo (`packages/`, `apps/`).
The two coexist at the repo root: pnpm ignores `crates/`/`target/`, cargo ignores
`packages/`. This is the **real native capture + input layer** that will eventually
back the TS `NativeLayer` seam (`packages/platform/src/desktop.ts`) via a napi-rs
binding (future round). It supersedes the JS/Python explorations in
`playground/nutjs` and `playground/pyautogui`, which only wrap X11/Win32 and "can
install but not run in a headless container".

> `vrover-omniparser` (screen-parsing / Set-of-Mark grounding) also lives here as a
> separate crate. It is the vision half and is independent of the driver layer below.

## One crate, `vrover-drivers`

There is now a **single** crate, [`drivers`](./drivers). Two independent traits live
in its core; the three platform backends are **feature-gated modules** within it
(default off, so the crate builds + tests with no native system libraries). They
used to be three standalone crates (`vrover-pipewire` / `-uinput` / `-libei`); each
implemented one trait and depended on `vrover-drivers`, so they were folded in.

- **`CaptureSource`** — produce screen frames (`Frame`, raw BGRA).
- **`InputSink`** — inject mouse + keyboard events.

They are decoupled because on a **dual-machine / capture-card** setup the frame
*source* and the input *sink* target different devices. The four target platforms:

| platform | capture (`CaptureSource`) | input (`InputSink`) |
|---|---|---|
| **Linux Wayland** (priority) | `pipewire` — xdg-desktop-portal ScreenCast | `uinput` (kernel virt. device), `libei` (portal emulated) |
| Windows (future) | DXGI Desktop Duplication / GDI | Win32 `SendInput` |
| capture card / dual-machine (future) | v4l2 video device | (separate return channel) |
| Android (future, dual-machine) | `adb exec-out screencap` | `adb shell input` |

| module (feature) | role | feature / target |
|---|---|---|
| [`drivers`](./drivers) core | `CaptureSource` + `InputSink` traits, `Frame`/`Button`/`Key`/`DriverError`, test stubs. **No platform deps** — always builds + tests. | — |
| [`backends::pipewire`](./drivers/src/backends/pipewire) | `CaptureSource` via PipeWire ScreenCast (ashpd + pipewire-rs). | `pipewire` feature (off by default) |
| [`backends::uinput`](./drivers/src/backends/uinput) | `InputSink` via the uinput kernel virtual device (evdev). Keycode map always compiles + tests. | `uinput` feature (off by default), Linux |
| [`backends::libei`](./drivers/src/backends/libei.rs) | `InputSink` via libei / xdg-desktop-portal emulated input. **Stub until libei is packaged.** | `libei` feature (off by default) |

## Build & test

Toolchain + system libs (one-time; the dev container has passwordless `sudo`):

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh
sh /tmp/rustup-init.sh -y --profile minimal
source "$HOME/.cargo/env"

# System libs. First line *builds* the pipewire backend; second line *runs* it
# (modules supply protocol-native + adapter; the `pipewire` pkg supplies the
# runtime client.conf that loads them).
sudo apt-get install -y libpipewire-0.3-dev libclang-dev clang libdbus-1-dev pkg-config
sudo apt-get install -y libpipewire-0.3-modules pipewire
```

```bash
cargo build --workspace                          # core + all stubs (always green)
cargo test --workspace                           # pure-logic: Frame/PNG, key/Button maps, trait defaults, stubs
cargo build --workspace --all-features           # compile EVERY real backend (evdev + pipewire + ashpd)
cargo build -p vrover-drivers --features uinput  # just the uinput backend
cargo build -p vrover-drivers --features pipewire# just the pipewire backend
```

## Capturable desktop in this container (verified 2026-06-24)

The dev container is **not** headless: `.devcontainer/devcontainer.json` bind-mounts
the host's `/tmp/.X11-unix` and `/run/user/1000`, so the real GNOME/Wayland desktop
(`gnome-shell` + `Xwayland :0/:1`), the PipeWire socket (`pipewire-0`), the full
`xdg-desktop-portal` stack (+gnome, +gtk), and `org.gnome.Mutter.ScreenCast` are all
reachable from inside. So the **pipewire capture path is run-testable here now.**

```bash
cargo run -p vrover-drivers --example capture_one --features pipewire -- /tmp/shot.png
```

`capture_one` negotiates an ashpd ScreenCast session, waits for the first PipeWire
frame, and writes it to PNG. **The portal pops a "select what to share" dialog on the
host desktop each run** (the backend uses `PersistMode::DoNot`) — approve it (pick the
monitor) and the PNG lands at the path you give. A verified capture produced a real
2560×1600 RGBA PNG of the live desktop.

Two notes for future work:
- **No per-run dialog:** the dialog-free route is to drive
  `org.gnome.Mutter.ScreenCast` directly over D-Bus (`CreateSession` → `RecordMonitor`
  → PipeWire node id), bypassing the portal — proven callable with no dialog. The
  portal path could also be made non-interactive by switching to `PersistMode::Persistent`
  + reusing the restore token.
- **DMA-BUF still TODO:** Mutter handed an mmap'd (memfd) buffer here, so the current
  decoder path works. A hardware/DMA-BUF path would need `SPA_DATA_DmaBuf` handling.
  Multi-monitor stream selection (we take stream 0) and cursor-mode/restore-token knobs
  on `PipeWireSourceBuilder` remain open (search `TODO(host)`).

## PipeWire backend — runtime dependencies (the part that bit us)

Building the pipewire backend and **running** it are two different dependency sets.
The `-dev` packages are build-only. At **runtime** a binary linked against
`libpipewire` additionally needs, on the target system:

| need | Debian/Ubuntu package | provides |
|---|---|---|
| the shared lib you link | `libpipewire-0.3-0t64` | `libpipewire-0.3.so.0` |
| SPA plugins (converters, support) | `libspa-0.2-modules` | `/usr/lib/<arch>/spa-0.2/{audioconvert,videoconvert,support,…}` |
| PipeWire client **modules** | `libpipewire-0.3-modules` | `/usr/lib/<arch>/pipewire-0.3/libpipewire-module-*.so` |
| the **client.conf** runtime config | `pipewire` | `/usr/share/pipewire/client.conf` (+ `client.conf.avail/`) |
| a graphical session (env, not a pkg) | — | running `pipewire` daemon + `xdg-desktop-portal` + a backend |

`pw_context_new` reads **`client.conf`** (search: `$PIPEWIRE_CONFIG_DIR` →
`~/.config/pipewire/` → `/etc/pipewire/` → `/usr/share/pipewire/`), which maps
`spa-libs` and loads `context.modules`. The `-dev` / `-common` packages do **not**
ship `client.conf` — only the **`pipewire`** (daemon) package does. Without it:
`can't load config client.conf: No such file or directory → pw Context::new: Creation failed`.

Two packaging strategies:
- **(a) Depend on the `pipewire` package** and use the system config. Simplest.
- **(b) Ship a minimal `client.conf`** and `PIPEWIRE_CONFIG_DIR=<dir>` it — then you
  only need the three library/module packages. A working minimal `client.conf` loads
  `libpipewire-module-protocol-native` + `-client-node` + `-adapter` (essential) and
  optionally `-metadata` / `-session-manager` / `-rt`.

Pitfalls (debug aid: `PIPEWIRE_DEBUG=3`):

| # | error | cause | fix |
|---|---|---|---|
| 1 | `can't load config client.conf` / `Context::new: Creation failed` | no `client.conf` | install `pipewire` pkg, or ship one + `PIPEWIRE_CONFIG_DIR` |
| 2 | `connect_fd: Creation failed` | `protocol-native` module not loaded | ensure `libpipewire-0.3-modules` + it's in `client.conf` |
| 3 | `stream.connect: EPROTO: Protocol error` | offered an **empty** format list | build a real SPA `EnumFormat` POD (done in `backend.rs`) |
| 4 | `no adapter factory found` / `can't make node: ENOENT` | `adapter` module not loaded | `client.conf` must load `libpipewire-module-adapter` |
| 5 | frames never decode, no error | node hands **DMA-BUF** buffers; only mmap'd path handled | TODO(host): `SPA_DATA_DmaBuf` |

## Relationship to the TS side (future round)

The traits line up 1:1 with `NativeLayer` (`packages/platform/src/desktop.ts`):

| TS `NativeLayer` | Rust |
|---|---|
| `captureScreen()` → `Screenshot{png}` | `CaptureSource::capture()` + `Frame::to_png()` |
| `performClick/Type/Scroll/Keypress` | `InputSink` methods |

A future **napi-rs binding crate** will compose a `CaptureSource` + `InputSink` into a
`NativeLayer` and hand it to `DesktopPlatform`, so the TS agent loop drives real
capture/input with no other changes. Grounding (`getAccessibilityElements` / AT-SPI,
or `vrover-omniparser`) is a separate concern and stays out of this layer.
