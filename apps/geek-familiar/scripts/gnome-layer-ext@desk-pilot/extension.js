/**
 * gnome-layer-ext@desk-pilot — keep the desktop-pet window always above other
 * windows AND completely excluded from Activities Overview / Alt-Tab.
 *
 * Two-pronged:
 * 1. Mutter-layer: make_above() + skip_taskbar/skip_pager (socket server).
 * 2. Shell Overview interception: override Workspace.WindowPreview creation
 *    to skip windows whose title starts with "desktop-pet#".  This is the
 *    only approach that works on Mutter 49 where Overview ignores Mutter
 *    skip_pager properties.
 *
 * Socket: $XDG_RUNTIME_DIR/gnome-layer-ext.sock
 */

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import St from 'gi://St';
import * as Workspace from 'resource:///org/gnome/shell/ui/workspace.js';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

const SOCK   = `${GLib.get_user_runtime_dir()}/gnome-layer-ext.sock`;
const PREFIX = 'desktop-pet#';

const _isPet = (w) => (w?.get_title?.() ?? '').startsWith(PREFIX);
const _tokenOf = (w) => { const t = w?.get_title?.() ?? ''; const i = t.indexOf(PREFIX); return i >= 0 ? t.slice(i + PREFIX.length) : null; };

// _ghost removed — skip-taskbar is read-only on MetaWindowWayland (Mutter 49).
// Overview exclusion is handled by _isOverviewWindow patch instead.

// ── Overview exclusion: override Workspace._windowAdded ─────────────────────

let _origIsOverviewWindow = null;

function _patch() {
    // GNOME 45+ ESM: `export default class Workspace` → imported as
    // `Workspace.default` with `import * as Workspace`.
    let proto = Workspace.default?.prototype;
    if (!proto?._isOverviewWindow)
        proto = Workspace.Workspace?.prototype;  // fallback
    if (!proto?._isOverviewWindow) {
        log(`[gnome-layer-ext] overview patch FAILED — cannot find _isOverviewWindow`);
        return;
    }
    if (proto.__patched) return;
    proto.__patched = true;

    _origIsOverviewWindow = proto._isOverviewWindow;
    proto._isOverviewWindow = function (win) {
        if (_isPet(win)) return false;  // pet windows never show in Overview
        return _origIsOverviewWindow.call(this, win);
    };
    print('[gnome-layer-ext] overview: _isOverviewWindow patched (pet windows excluded)');
}

function _unpatch() {
    let proto = Workspace.default?.prototype ?? Workspace.Workspace?.prototype;
    if (_origIsOverviewWindow && proto) {
        proto._isOverviewWindow = _origIsOverviewWindow;
        proto.__patched = false;
        _origIsOverviewWindow = null;
    }
}

// ── Extension ───────────────────────────────────────────────────────────────

export default class PetOnTopExtension extends Extension {
    enable() {
        // Shell-level Overview exclusion
        // _patch();  // kept for reference — disable overview exclusion

        this._pending = new Set();
        this._pinned  = new Set();
        this._clipSubs = new Set();  // persistent connections for clipboard push

        // Mutter-layer hints on window-created
        this._winCreatedId = global.display.connect('window-created', (_d, w) => {
            if (!_isPet(w)) return;
            try { if (!w.is_above()) w.make_above(); } catch (_) {}
            // Show on all workspaces.
            if (typeof w.stick === 'function') w.stick();
        });

        // ── Clipboard change subscription ─────────────────────────
        // On clipboard change, read the actual content and push it
        // to subscribed clients (the pet runs in a container and
        // cannot access the host clipboard directly).
        try {
            this._clipboard = St.Clipboard.get_default();
            const sel = global.display.get_selection();
            const CLIP = Meta.SelectionType?.SELECTION_CLIPBOARD ?? 1;
            this._clipSelId = sel.connect('owner-changed', (_s, type, _src) => {
                if (type !== CLIP) return;
                print(`[gnome-layer-ext] clipboard changed, subs=${this._clipSubs.size}`);
                this._clipboard.get_text(St.ClipboardType.CLIPBOARD, (_c, text) => {
                    const t = text || '';
                    print(`[gnome-layer-ext] clipboard text="${t.substring(0,40)}"`);
                    const payload = JSON.stringify({type:'clipboard',text:t}) + '\n';
                    for (const conn of this._clipSubs) {
                        try { conn.get_output_stream().write_bytes(new TextEncoder().encode(payload), null); }
                        catch (_) { this._clipSubs.delete(conn); }
                    }
                });
            });
            print('[gnome-layer-ext] clipboard subscription active');
        } catch (e) { log(`[gnome-layer-ext] clipboard subscription failed: ${e}`); }

        // Socket server
        try { GLib.unlink(SOCK); } catch (_) {}
        this._service = new Gio.SocketService();
        this._service.add_address(
            Gio.UnixSocketAddress.new(SOCK),
            Gio.SocketType.STREAM, Gio.SocketProtocol.DEFAULT, null);
        this._incId = this._service.connect('incoming', (_s, c) => {
            const input = Gio.DataInputStream.new(c.get_input_stream());
            input.read_line_async(GLib.PRIORITY_DEFAULT, null, (stream, res) => {
                let token = null, subscribe = false;
                try {
                    const r = stream.read_line_finish(res);
                    const bytes = Array.isArray(r) ? r[0] : r;
                    const line = bytes ? new TextDecoder().decode(bytes) : '';
                    const obj = JSON.parse(line.trim()) ?? {};
                    token = obj.token ?? null;
                    subscribe = obj.subscribe_clipboard === true;
                } catch (_) { token = null; }
                let ok = !!token;
                if (token) {
                    let pinned = false;
                    for (const actor of global.get_window_actors()) {
                        const w = actor.meta_window;
                        if (w && w.window_type === Meta.WindowType.NORMAL && _tokenOf(w) === token) {
                            try { if (!w.is_above()) w.make_above(); } catch (_) {}
                            if (typeof w.stick === 'function') w.stick();
                            this._pinned.add(token); pinned = true; break;
                        }
                    }
                    if (!pinned) this._pending.add(token);
                }
                const out = c.get_output_stream();
                out.write_all_async(new TextEncoder().encode(`{"ok":${ok}}\n`),
                    GLib.PRIORITY_DEFAULT, null, (o, r) => {
                        try { o.write_all_finish(r); } catch (_) {}
                        if (subscribe && ok) {
                            // Keep connection open for clipboard push.
                            this._clipSubs.add(c);
                            print('[gnome-layer-ext] clipboard subscriber added');
                        } else {
                            c.close(null);
                        }
                    });
            });
        });
        print('[gnome-layer-ext] enabled');
    }

    disable() {
        _unpatch();
        if (this._winCreatedId) { global.display.disconnect(this._winCreatedId); this._winCreatedId = null; }
        if (this._clipSelId) { global.display.get_selection().disconnect(this._clipSelId); this._clipSelId = null; }
        if (this._incId && this._service) { this._service.disconnect(this._incId); this._incId = null; }
        if (this._service) { this._service.stop(); this._service = null; }
        try { GLib.unlink(SOCK); } catch (_) {}
        for (const c of this._clipSubs) { try { c.close(null); } catch (_) {} }
        this._clipSubs.clear();
        for (const actor of global.get_window_actors()) {
            const w = actor.meta_window;
            if (w && this._pinned.has(_tokenOf(w))) {
                if (w.is_above()) w.unmake_above();
                if (typeof w.unstick === 'function') w.unstick();
            }
        }
        this._pinned.clear(); this._pending.clear();
    }
}
