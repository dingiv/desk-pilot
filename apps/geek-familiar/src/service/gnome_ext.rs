//! gnome-layer-ext handshake — ask the GNOME Shell extension to pin this window
//! (always-on-top + skip-taskbar/pager + Activities/Alt-Tab exclusion).
//!
//! PUSH MODEL (matches the extension at
//! `~/.local/share/gnome-shell/extensions/gnome-layer-ext@vrover/`): the extension
//! listens on a Unix socket and only pins windows that explicitly ask. The app
//! sets its window title to `desktop-pet#<token>`, connects here, and sends
//! `{"v":1,"token":"<token>","app_id":"..."}`. The extension finds the
//! `Meta.Window` by that title token and `make_above()` + skip_taskbar/pager.
//!
//! Wayland's xdg-shell dropped the skip-taskbar hint and winit's `WindowLevel`
//! is a no-op on Wayland, so this socket is the only place always-on-top +
//! ghosting can be set. The window stays type NORMAL so compositor drag still
//! works.
//!
//! Install the extension on the GNOME host: `apps/geek-familiar/scripts/install-ext.sh`
//! (source: `apps/geek-familiar/scripts/gnome-layer-ext@vrover/`).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// One-shot handshake over the gnome-layer-ext socket.
///
/// Returns `true` if the extension replied `{"ok":true}` (window pinned). The
/// caller must have already set the window title to `desktop-pet#<token>` so the
/// extension can find the window.
pub async fn handshake(token: String, app_id: &str) -> bool {
    let sock = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/gnome-layer-ext.sock"))
        .unwrap_or_else(|_| "/run/user/1000/gnome-layer-ext.sock".into());
    match UnixStream::connect(&sock) {
        Ok(mut s) => {
            let req = format!("{{\"v\":1,\"token\":\"{token}\",\"app_id\":\"{app_id}\"}}\n");
            if s.write_all(req.as_bytes()).is_err() {
                return false;
            }
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            resp.contains("\"ok\":true")
        }
        Err(_) => false,
    }
}
