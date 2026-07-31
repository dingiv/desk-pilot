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

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// One-shot handshake over the gnome-layer-ext socket.
pub async fn handshake(token: String, app_id: &str) -> bool {
    let sock = socket_path();
    match UnixStream::connect(&sock) {
        Ok(mut s) => {
            let req = format!("{{\"v\":1,\"token\":\"{token}\",\"app_id\":\"{app_id}\"}}\n");
            if s.write_all(req.as_bytes()).is_err() { return false; }
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            resp.contains("\"ok\":true")
        }
        Err(_) => false,
    }
}

/// Persistent socket connection for clipboard push from the extension.
/// The extension reads the clipboard content host-side (the pet runs in a
/// container and can't access it directly) and pushes
/// `{"type":"clipboard","text":"..."}\n` on each change.
pub fn subscribe_clipboard(
    token: String, mut on_text: impl FnMut(String) + Send + 'static,
) {
    let sock = socket_path();
    let req = format!("{{\"v\":1,\"token\":\"{token}\",\"app_id\":\"geek-familiar\",\"subscribe_clipboard\":true}}\n");
    eprintln!("[geek-familiar] clipboard: subscribing via socket {sock}");
    let _ = std::thread::Builder::new().name("familiar-clipboard".into()).spawn(move || {
        loop {
            if let Ok(mut s) = UnixStream::connect(&sock) {
                eprintln!("[geek-familiar] clipboard: socket connected");
                let _ = s.write_all(req.as_bytes());
                for line in BufReader::new(&mut s).lines() {
                    match line {
                        Ok(l) if l.contains("\"type\":\"clipboard\"") => {
                            if let Some(text) = serde_json::from_str::<serde_json::Value>(&l).ok()
                                .and_then(|v| v.get("text")?.as_str().map(|s| s.to_string()))
                            {
                                eprintln!("[geek-familiar] clipboard: received from ext len={}", text.len());
                                on_text(text);
                            }
                        }
                        Ok(l) => eprintln!("[geek-familiar] clipboard: recv {l}"),
                        Err(_) => { eprintln!("[geek-familiar] clipboard: socket disconnected, retrying..."); break; }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn socket_path() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/gnome-layer-ext.sock"))
        .unwrap_or_else(|_| "/run/user/1000/gnome-layer-ext.sock".into())
}
