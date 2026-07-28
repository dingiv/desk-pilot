#!/usr/bin/env bash
# Install gnome-layer-ext@vrover to the GNOME host's per-user extensions dir and
# enable it. The geek-familiar pet pings this extension's socket at boot to get
# always-on-top + Activities/taskbar/Alt-Tab exclusion on GNOME/Wayland.
#
# Run from anywhere; resolves the host extensions dir automatically:
#   - dev container: the GNOME session's home is bind-mounted at /home/host
#   - on the host itself: $HOME
#
# After install: if the extension was already loaded, restart GNOME Shell to
# pick up changes (X11: Alt+F2 → r; Wayland: relogin). First-time enable is live.
set -euo pipefail

EXT_UUID="gnome-layer-ext@vrover"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$SCRIPT_DIR/$EXT_UUID"

if [[ ! -d "$SRC" ]]; then
    echo "[install-ext] source not found: $SRC" >&2
    exit 1
fi

# Resolve the GNOME host's extensions dir.
if [[ -d /home/host/.local/share/gnome-shell/extensions ]]; then
    DEST=/home/host/.local/share/gnome-shell/extensions
elif [[ -n "${GNOME_EXT_DIR:-}" ]]; then
    DEST="$GNOME_EXT_DIR"
else
    DEST="$HOME/.local/share/gnome-shell/extensions"
fi

mkdir -p "$DEST"
rm -rf "$DEST/$EXT_UUID"
cp -r "$SRC" "$DEST/$EXT_UUID"
echo "[install-ext] copied → $DEST/$EXT_UUID"

# Enable (live on Wayland). Disable first so re-enabling after an update is clean.
gnome-extensions disable "$EXT_UUID" 2>/dev/null || true
if gnome-extensions enable "$EXT_UUID" 2>/dev/null; then
    echo "[install-ext] enabled $EXT_UUID"
else
    echo "[install-ext] could not enable via gnome-extensions (no live GNOME session here?)."
    echo "               Run this on the GNOME host, or enable via the Extensions app."
fi

echo
echo "[install-ext] done. State:"
gnome-extensions info "$EXT_UUID" 2>/dev/null || \
    echo "  (gnome-extensions not available in this environment — check on the host)"
echo
echo "If the extension was already loaded, restart GNOME Shell to reload it"
echo "(X11: Alt+F2 → r; Wayland: log out + back in)."
