# gnome-layer-ext@desk-pilot — GNOME Shell Extension

Keeps the **geek-familiar** window pinned above, on all workspaces, and optionally
out of the Activities Overview. Works via Mutter API + Shell-level interception.

## Features
- **Always-on-top** — `Meta.Window.make_above()`
- **Sticky (all workspaces)** — `Meta.Window.stick()`, so the pet follows you
  across workspace switches
- **Overview exclusion** (optional) — `Workspace._isOverviewWindow` patch to
  skip windows whose title starts with `desktop-pet#`

## How it works (push, server)
- Unix socket at `$XDG_RUNTIME_DIR/gnome-layer-ext.sock`
- App sets title `desktop-pet#<token>`, connects, sends `{"v":1,"token":"<token>","app_id":"geek-familiar"}`
- Reply: `{"ok":true}`

## Install
```bash
cp -r gnome-layer-ext@desk-pilot ~/.local/share/gnome-shell/extensions/
```
Restart GNOME Shell after install (Wayland: log out; X11: Alt+F2 → r).

## Notes
- Targets GNOME Shell **49** (`shell-version`).
- Overview exclusion (`_patch` / `_isOverviewWindow`) is **disabled** by default
  (commented out). Enable it by uncommenting `_patch()` in `extension.js`.
- `skip-taskbar` is **read-only** on Wayland (`MetaWindowWayland`) — this
  extension does not attempt to set it.
