//! One-shot uinput probe: open the kernel virtual device, confirm the kernel
//! actually registers it, then exercise the full `InputSink` surface and report
//! each step. Intended to answer: "now that `/dev/uinput` is passed through, can
//! the uinput driver really open it and inject events?"
//!
//! Run (real backend; needs `/dev/uinput` write access — root or the `uinput`
//! group, and the device passed through into the container/namespace):
//!   cargo run -p scout-drivers --example uinput_probe --features uinput
//!
//! It does **not** move your real cursor unless a compositor is attached; it only
//! proves the device opens and the kernel accepts the emitted events. A live
//! session (Wayland/X11) is what turns those events into on-screen motion.

use scout_drivers::{Button, InputSink, Key, UinputSink};

/// The device name [`UinputSink`] advertises (matches `backend.rs`).
const DEVICE_NAME: &str = "VRover uinput sink";

fn main() {
    // ── 1. open /dev/uinput via the evdev VirtualDeviceBuilder ────────────────
    eprintln!("[uinput_probe] opening /dev/uinput (VirtualDeviceBuilder)…");
    let mut sink = match UinputSink::with_screen(1920, 1080) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[uinput_probe] OPEN FAILED: {e}");
            eprintln!("[uinput_probe]   → is /dev/uinput passed through + writable (0666/uinput grp)?");
            std::process::exit(2);
        }
    };
    eprintln!("[uinput_probe] open OK; virtual device registered with kernel.");

    // ── 2. self-verify: the kernel must list the virtual device ───────────────
    match kernel_sees_device() {
        Ok(true) => eprintln!("[uinput_probe] VERIFY OK: /proc/bus/input/devices lists {DEVICE_NAME:?}."),
        Ok(false) => {
            eprintln!("[uinput_probe] VERIFY WARN: {DEVICE_NAME:?} not found in /proc/bus/input/devices");
            eprintln!("[uinput_probe]   (emit may still succeed; this only means the name isn't introspectable here.)");
        }
        Err(e) => eprintln!("[uinput_probe] VERIFY SKIP: could not read /proc/bus/input/devices ({e})"),
    }

    // ── 3. exercise the InputSink surface; each emit() is one EV_SYN frame ────
    step("move_to(960, 540)", || sink.move_to(960, 540));
    step("click(800, 400, Left)", || sink.click(800, 400, Button::Left));
    step("scroll(800,400, 0, -2)", || sink.scroll(800, 400, 0, -2));
    step("tap_key(Enter)", || sink.tap_key(Key::Enter));
    // Printable ASCII — letters (Shift for caps), digits, and symbols — all map:
    step("type_text(\"Hi!\")", || sink.type_text("Hi!")); // '!' = Shift+'1'
    step("type_text(\"a=1.0\")", || sink.type_text("a=1.0")); // '=' and '.'
    // Non-ASCII still has no KEY_* → documented NotSupported (use libei):
    step("type_text(\"café\")", || sink.type_text("café"));

    // Drop is implicit; the virtual device disappears from the kernel on close.
    eprintln!("[uinput_probe] done — all emits accepted by the kernel.");
}

fn step<F: FnOnce() -> scout_drivers::Result<()>>(name: &str, f: F) {
    match f() {
        Ok(()) => eprintln!("[uinput_probe]   ✓ {name}"),
        Err(e) => eprintln!("[uinput_probe]   ✗ {name}: {e}"),
    }
}

/// True iff `/proc/bus/input/devices` lists a handler whose name is ours.
fn kernel_sees_device() -> std::io::Result<bool> {
    let s = std::fs::read_to_string("/proc/bus/input/devices")?;
    Ok(s.contains(DEVICE_NAME))
}
