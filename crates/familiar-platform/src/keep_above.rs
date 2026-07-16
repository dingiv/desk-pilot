//! Linux/Wayland "keep above" strategies. `PetWindow::request_keep_above`
//! (GTK impl) delegates here.
//!
//! - [`LayerShellStrategy`]     — wlroots/KDE: `gtk4-layer-shell` overlay layer (stub).
//! - [`GnomeExtensionStrategy`] — GNOME: asks the `gnome-layer-ext@vrover` Shell
//!   extension (running as a Unix-socket server) to `make_above()` our window.
//!
//! GNOME is **push-based**: the app identifies its window by a token in the
//! title (`geek-familiar#<token>`) and sends it over
//! `$XDG_RUNTIME_DIR/gnome-layer-ext.sock`. The extension matches the title and
//! pins. Title-matching is PID-namespace-agnostic (a dev container may see a
//! different pid than the host compositor).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::window::KeepAboveResult;

/// A pin request: which app, and which window (title token).
#[derive(Clone, Debug)]
pub struct PinRequest<'a> {
    pub app_id: &'a str,
    /// The token embedded in the window title as `geek-familiar#<token>`.
    pub token: &'a str,
}

/// How a Linux/Wayland backend tries to keep the pet above other windows.
pub trait KeepAboveStrategy {
    /// Human-readable id, e.g. `"gnome-extension"`, `"layer-shell"`.
    fn id(&self) -> &'static str;
    /// Try to keep the window described by `req` on top.
    fn enable(&self, req: &PinRequest) -> KeepAboveResult;
    /// Release keep-above (no-op if never applied).
    fn disable(&self) {}
}

/// wlroots/KDE: place the window in the overlay layer via `gtk4-layer-shell`.
///
/// TODO(M2): add `gtk4-layer-shell` dep; on a wlroots/KDE compositor set
/// `Layer::Overlay`, anchor = none, exclusive-zone = 0, keyboard interactivity
/// none. Mutter doesn't implement wlr-layer-shell, so this is GNOME-inert.
pub struct LayerShellStrategy;

impl KeepAboveStrategy for LayerShellStrategy {
    fn id(&self) -> &'static str {
        "layer-shell"
    }
    fn enable(&self, _req: &PinRequest) -> KeepAboveResult {
        KeepAboveResult::Unsupported // not wired yet
    }
}

/// GNOME: ask the `gnome-layer-ext@vrover` Shell extension (socket server) to pin us.
///
/// The extension must be installed + enabled on the host (see
/// `scripts/gnome-layer-ext/README.md`). `enable()` connects to
/// `$XDG_RUNTIME_DIR/gnome-layer-ext.sock`, sends `{token, app_id}`, and reads the
/// reply. Returns `Unsupported` if the extension isn't running (connect failed)
/// or replied `ok:false`.
pub struct GnomeExtensionStrategy;

impl KeepAboveStrategy for GnomeExtensionStrategy {
    fn id(&self) -> &'static str {
        "gnome-extension"
    }
    fn enable(&self, req: &PinRequest) -> KeepAboveResult {
        request_pin(req)
    }
}

/// Pick a strategy from the running session: GNOME → extension; else layer-shell.
pub fn detect() -> Box<dyn KeepAboveStrategy> {
    if is_gnome() {
        Box::new(GnomeExtensionStrategy)
    } else {
        Box::new(LayerShellStrategy)
    }
}

fn socket_path() -> String {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    format!("{dir}/gnome-layer-ext.sock")
}

fn request_pin(req: &PinRequest) -> KeepAboveResult {
    let mut stream = match UnixStream::connect(socket_path()) {
        Ok(s) => s,
        Err(_) => return KeepAboveResult::Unsupported, // extension not enabled
    };
    // small JSON line; tokenize manually to avoid a serde dependency
    let line = format!(
        "{{\"v\":1,\"token\":\"{}\",\"app_id\":\"{}\"}}\n",
        req.token, req.app_id
    );
    if stream.write_all(line.as_bytes()).is_err() {
        return KeepAboveResult::Unsupported;
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);

    // read the reply (best-effort, short)
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 128];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return KeepAboveResult::Unsupported,
    };
    let reply = std::str::from_utf8(&buf[..n]).unwrap_or("");
    if reply.contains("\"ok\":true") {
        KeepAboveResult::Applied
    } else {
        KeepAboveResult::Unsupported
    }
}

fn is_gnome() -> bool {
    // XDG_CURRENT_DESKTOP contains "GNOME" on GNOME sessions. If unset, assume
    // GNOME (the more constrained path) so we don't promise layer-shell.
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.to_ascii_uppercase().contains("GNOME"))
        .unwrap_or(true)
}
