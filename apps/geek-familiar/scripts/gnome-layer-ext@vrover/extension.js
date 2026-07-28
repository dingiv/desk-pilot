/**
 * gnome-layer-ext@vrover — keep the desktop-pet window always above other
 * windows AND out of the Activities Overview / taskbar / Alt-Tab.
 *
 * PUSH MODEL (server). The extension listens on a Unix socket and only pins
 * windows that explicitly ask. The pet app sets its window title to
 * `desktop-pet#<token>` (borderless, so invisible), connects to the socket, and
 * sends `{"v":1,"token":"<token>","app_id":"..."}`. The extension finds the
 * `Meta.Window` by that title token and:
 *   1. `make_above()` — Mutter's "above" stacking layer (one-time, ~zero cost).
 *   2. skip_taskbar + skip_pager — turns it into a desktop "ghost": the
 *      Activities Overview (Win/Super), the taskbar, Alt-Tab, and the workspace
 *      pager all ignore it, so pressing Super no longer tiles the pet with the
 *      other windows. Wayland's xdg-shell dropped the skip-taskbar hint and GTK4
 *      its client API, so this is the only place it can be set. The window stays
 *      type NORMAL (so the compositor drag / begin_move still works).
 *
 * If the request arrives before the window is mapped, the token is queued and
 * pinned on the next `window-created`.
 *
 * Socket: $XDG_RUNTIME_DIR/gnome-layer-ext.sock (shared across the host + a dev
 * container that bind-mounts /run/user/<uid>).
 */

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

const SOCK = `${GLib.get_user_runtime_dir()}/gnome-layer-ext.sock`;
const PREFIX = 'desktop-pet#';

export default class PetOnTopExtension extends Extension {
    enable() {
        this._pending = new Set(); // tokens requested before their window appeared
        this._pinned = new Set();  // tokens we've made_above

        // Satisfy requests that arrived before the window was mapped.
        this._createdId = global.display.connect('window-created', (_d, w) => {
            for (const token of [...this._pending])
                if (this._tryPin(w, token)) {
                    this._pending.delete(token);
                    this._pinned.add(token);
                }
        });

        try { GLib.unlink(SOCK); } catch (e) { /* may not exist */ }

        this._service = new Gio.SocketService();
        // GIO (GNOME 49) requires a GSocketAddress here, not a string.
        this._service.add_address(
            Gio.UnixSocketAddress.new(SOCK),
            Gio.SocketType.STREAM,
            Gio.SocketProtocol.DEFAULT,
            null,
        );
        this._incomingId = this._service.connect('incoming', (_svc, conn) =>
            this._handle(conn));
    }

    disable() {
        if (this._createdId) {
            global.display.disconnect(this._createdId);
            this._createdId = null;
        }
        if (this._incomingId && this._service) {
            this._service.disconnect(this._incomingId);
            this._incomingId = null;
        }
        if (this._service) {
            this._service.stop();
            this._service = null;
        }
        try { GLib.unlink(SOCK); } catch (e) {}

        for (const actor of global.get_window_actors()) {
            const w = actor.meta_window;
            if (w && this._pinned.has(this._tokenOf(w))) {
                this._ghost(w, false);
                if (w.is_above()) w.unmake_above();
            }
        }
        this._pinned.clear();
        this._pending.clear();
    }

    _handle(conn) {
        const input = Gio.DataInputStream.new(conn.get_input_stream());
        input.read_line_async(GLib.PRIORITY_DEFAULT, null, (stream, res) => {
            let token = null;
            try {
                const r = stream.read_line_finish(res);
                const bytes = Array.isArray(r) ? r[0] : r;
                const line = bytes ? new TextDecoder().decode(bytes) : '';
                token = (JSON.parse(line.trim()) ?? {}).token ?? null;
            } catch (e) {
                token = null;
            }

            let ok = !!token;
            if (token) {
                let pinned = false;
                for (const actor of global.get_window_actors())
                    if (this._tryPin(actor.meta_window, token)) { pinned = true; break; }
                if (pinned) this._pinned.add(token);
                else this._pending.add(token); // accepted; will pin on window-created
            }

            const out = conn.get_output_stream();
            // newer gjs wants a Uint8Array for the buffer, not a string:
            out.write_all_async(
                new TextEncoder().encode(`{"ok":${ok}}\n`),
                GLib.PRIORITY_DEFAULT,
                null,
                (o, res2) => {
                    try { o.write_all_finish(res2); } catch (e) {}
                    conn.close(null);
                },
            );
        });
    }

    _tryPin(win, token) {
        if (!win || win.window_type !== Meta.WindowType.NORMAL) return false;
        if (this._tokenOf(win) !== token) return false;
        if (!win.is_above()) win.make_above();
        this._ghost(win, true);
        return true;
    }

    /**
     * Set/clear the desktop-"ghost" hints (skip_taskbar / skip_pager). Mutter
     * (MR !1056) lets these be set; the binding form differs by version, so try
     * the writable GObject property first, then the explicit method. A failure
     * is logged but never breaks pinning. Window type stays NORMAL so the
     * compositor drag still works.
     */
    _ghost(win, on) {
        const props = on
            ? [['skip_taskbar', 'skip_from_taskbar'], ['skip_pager', 'skip_from_pager']]
            : [['skip_taskbar', 'show_in_taskbar'], ['skip_pager', 'show_in_pager']];
        for (const [prop, method] of props) {
            try {
                win[prop] = on;
                continue;
            } catch (e) { /* property not writable on this Mutter — try the method */ }
            if (typeof win[method] === 'function') {
                try {
                    win[method]();
                } catch (e2) {
                    log(`[gnome-layer-ext] ${method}() failed: ${e2}`);
                }
            } else if (on) {
                log(`[gnome-layer-ext] cannot set ${prop} on this Mutter`);
            }
        }
    }

    _tokenOf(win) {
        const t = (win.get_title?.() ?? '') ?? '';
        const i = t.indexOf(PREFIX);
        return i >= 0 ? t.slice(i + PREFIX.length) : null;
    }
}
