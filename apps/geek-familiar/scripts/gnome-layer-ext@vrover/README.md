# gnome-layer-ext@vrover — GNOME Shell extension

Keeps the **geek-familiar** window always above other normal windows on GNOME/Wayland,
using a **push model**: the app asks to be pinned and tells the extension which
window it is. Also hides it from the Activities Overview / taskbar / Alt-Tab.

## Why
GNOME Wayland gives clients no way to request always-on-top (no compositor
protocol; `winit`'s `WindowLevel::AlwaysOnTop` is a no-op on Wayland). The
compositor (Mutter) can set a window's stacking layer internally, though. This
extension calls `Meta.Window.make_above()` on the pet — placing it in Mutter's
"above" layer, maintained by the compositor's own stack sync (a **one-time
property**, not per-focus raising; ~zero cost). It also sets `skip_taskbar` /
`skip_pager` (the desktop-"ghost" hints Mutter MR !1056 added), which Wayland's
xdg-shell dropped — so the Activities Overview, taskbar, Alt-Tab, and workspace
pager all ignore the pet. Window type stays NORMAL so compositor drag still works.

## How it works (push, server)
- The extension runs a **Unix-socket server** at
  `$XDG_RUNTIME_DIR/gnome-layer-ext.sock` (shared across the host + a dev
  container that bind-mounts `/run/user/<uid>`).
- The pet app embeds a token in its (borderless, invisible) window title:
  `desktop-pet#<token>`, then connects and sends one JSON line:
  ```json
  {"v":1,"token":"<token>","app_id":"geek-familiar"}
  ```
- The extension finds the `Meta.Window` whose title starts with `desktop-pet#` +
  that token and calls `make_above()` + ghosts it. If the request arrives before
  the window is mapped, the token is queued and pinned on the next
  `window-created`.
- Reply: `{"ok":true}` (request accepted) or `{"ok":false}` (bad request).

The Rust client lives in `apps/geek-familiar/src/gnome_ext.rs` and runs the
handshake once at boot as an `iced::Task`.

## Install (on the GNOME host)
One command from the repo root (handles the dev-container host home at
`/home/host`, or `$HOME` on the host):
```bash
./apps/geek-familiar/scripts/install-ext.sh
```
Or manually:
```bash
mkdir -p ~/.local/share/gnome-shell/extensions
cp -r apps/geek-familiar/scripts/gnome-layer-ext@vrover \
      ~/.local/share/gnome-shell/extensions/
gnome-extensions enable gnome-layer-ext@vrover      # live on Wayland, no relogin
```

Then launch the pet (`cargo run -p geek-familiar --release`); it pings the socket
automatically and logs `[geek-familiar] gnome-layer-ext handshake ok=true`.

## Verify
- Pet's stderr shows `[geek-familiar] gnome-layer-ext handshake ok=true`.
- Drag another normal window (terminal/browser) over the pet → pet stays on top.
- Press Super (Activities) → the pet is NOT tiled with the other windows; it's not
  in the taskbar or Alt-Tab.
- `gnome-extensions info gnome-layer-ext@vrover` → State: ENABLED.

## Notes / limits
- Targets GNOME Shell **49** (`shell-version`). The APIs (`make_above`,
  `window-created`, class-based ESM `Extension`, `Gio.SocketService`,
  `skip_taskbar`/`skip_pager`) are stable since ~45; broaden `shell-version` if
  needed.
- Fullscreen apps still cover the pet (by design — same as any always-on-top).
- `disable()` releases (`unmake_above`) the windows it pinned and unlinks the
  socket.
