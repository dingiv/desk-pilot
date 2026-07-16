//! GTK4 backend (Linux/Wayland): a transparent, borderless window whose
//! `Picture` shows the pet's RGBA8 framebuffer, refreshed each frame.
//! ESC closes. Window-property ops (input region, keep-above) go through the
//! cross-platform [`PetWindow`][crate::window::PetWindow] trait; keep-above
//! delegates to the Linux [`KeepAboveStrategy`][crate::keep_above::KeepAboveStrategy]
//! (layer-shell vs GNOME extension).

use std::cell::RefCell;
use std::rc::Rc;
#[cfg(not(feature = "egui"))]
use std::time::Duration;

use core::Canvas;
#[cfg(not(feature = "egui"))]
use core::geometry::Vec2;
use gtk4::cairo;
use gtk4::gdk::{self, MemoryFormat, MemoryTexture};
use gtk4::glib::translate::ToGlibPtr;
use gtk4::glib::{Bytes, ControlFlow, Propagation};
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, CssProvider, EventControllerKey, EventControllerMotion,
    GestureClick, Picture,
};

use crate::keep_above::KeepAboveStrategy;
use crate::window::{InputRegion, KeepAboveMode, KeepAboveResult, PetWindow};
use crate::{App, PlatformBackend};
#[cfg(not(feature = "egui"))]
use crate::{MouseButton, PlatformEvent};

const APP_ID: &str = "org.vrover.GeekFamiliar";

pub struct GtkBackend;

impl GtkBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GtkBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformBackend for GtkBackend {
    fn run(&mut self, app: Box<dyn App>) -> ! {
        // Shared across GTK callbacks (all fire on the GTK main thread).
        let app: Rc<RefCell<Box<dyn App>>> = Rc::new(RefCell::new(app));

        let gtk_app = Application::new(Some(APP_ID), Default::default());
        let app_for_window = Rc::clone(&app);
        gtk_app.connect_activate(move |gtk_app| {
            build_window(gtk_app, &app_for_window);
        });

        gtk_app.run();
        std::process::exit(0);
    }
}

fn build_window(gtk_app: &Application, app: &Rc<RefCell<Box<dyn App>>>) {
    let win = ApplicationWindow::new(gtk_app);
    // Token in the (invisible, borderless) title lets the GNOME extension
    // identify THIS window when we ask it to pin us. geek-familiar#<pid>
    let token = std::process::id().to_string();
    win.set_title(Some(&format!("geek-familiar#{token}")));
    win.set_decorated(false);
    // GTK4 dropped `set_keep_above`/`set_skip_*` — see keep_above.rs + docs §10.

    let (w, h) = app.borrow().canvas_size();
    win.set_default_size(w as i32, h as i32);

    // Transparent background on Wayland: strip GTK's opaque theme background.
    let css = CssProvider::new();
    let _ = css.load_from_data("window, picture { background: transparent; }");
    gtk4::style_context_add_provider_for_display(
        &WidgetExt::display(&win),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_USER,
    );

    let picture = Picture::new();
    win.set_child(Some(&picture));

    // first frame before the loop starts
    paint_frame(app, &picture);

    // Drag: press on the pet → Mutter moves the *whole window*. Wayland forbids
    // client-side absolute positioning, but the compositor-driven *interactive*
    // move (xdg-toplevel move) is allowed, and that's how the pet relocates
    // across the desktop. On press we hand off: `gdk_toplevel_begin_move` gives
    // Mutter the press event's device + serial-derived timestamp, and Mutter
    // drags the window under the pointer until release. The click-through input
    // region (only the pet silhouette receives the press) keeps transparent areas
    // from starting a move. We also flip the FSM to Drag on press (pauses idle
    // drift while the window is being carried) and back to Idle on release /
    // stopped (resumes drift).
    #[cfg(not(feature = "egui"))]
    {
        let click = GestureClick::new();
        let app_press = Rc::clone(app);
        let win_for_move = win.clone();

        click.connect_pressed(move |gesture, _n, x, y| {
            // pause idle drift for the duration of the compositor move
            app_press
                .borrow_mut()
                .handle_event(&PlatformEvent::PointerDown {
                    button: MouseButton::Left,
                    pos: Vec2::new(x as f32, y as f32),
                });
            // Hand the interactive move to Mutter (xdg-toplevel move). Needs the
            // press device + button + timestamp (GTK maps the timestamp to the
            // wl_pointer serial). gdk_toplevel_begin_move has a runtime
            // GDK_IS_TOPLEVEL guard; our ApplicationWindow surface always is one,
            // so the GdkSurface→GdkToplevel pointer cast (same GObject instance)
            // is sound. gtk4-rs 0.11 hides `ToplevelExt::begin_move` down a
            // non-public path, so call the C symbol directly.
            let Some(device) = gesture.current_event_device() else {
                return;
            };
            let Some(surface) = win_for_move.surface() else {
                return;
            };
            unsafe {
                let surface_ptr: *mut gtk4::gdk::ffi::GdkSurface = surface.to_glib_none().0;
                let device_ptr: *mut gtk4::gdk::ffi::GdkDevice = device.to_glib_none().0;
                gtk4::gdk::ffi::gdk_toplevel_begin_move(
                    surface_ptr as *mut gtk4::gdk::ffi::GdkToplevel,
                    device_ptr,
                    gesture.current_button() as i32,
                    x,
                    y,
                    gesture.current_event_time(),
                );
            }
        });

        // resume drift when the move/click ends. `released` = a clean click
        // (press+release, no compositor grab); `stopped` = Mutter grabbed /
        // cancelled the sequence for the interactive move.
        let resume = |app: &Rc<RefCell<Box<dyn App>>>| {
            app.borrow_mut()
                .handle_event(&PlatformEvent::PointerUp {
                    button: MouseButton::Left,
                    pos: Vec2::new(0.0, 0.0),
                });
        };
        let app_release = Rc::clone(app);
        click.connect_released(move |_g, _n, _x, _y| {
            resume(&app_release);
        });
        let app_stop = Rc::clone(app);
        click.connect_stopped(move |_g| {
            resume(&app_stop);
        });
        win.add_controller(click);
    }

    // animation: repaint each frame (~vsync). Two compile-time paths:
    //   `egui` feature  → egui renders the UI to a buffer (offscreen wgpu) → blit
    //   otherwise        → the App's CPU renderer paints the Scene → blit
    // Both upload an RGBA8 buffer to the same `Picture`.
    #[cfg(feature = "egui")]
    {
        let picture = picture.clone();
        let (cw, ch) = (w, h);
        let surface = Rc::new(RefCell::new(None::<crate::gtk_egui::EguiSurface>));
        let app = Rc::clone(app);
        // retained text-edit buffers (per ui::Id) so egui's cursor persists
        let scratch = Rc::new(RefCell::new(std::collections::HashMap::<ui::Id, String>::new()));
        // resized pet images, keyed by (src, display w, display h)
        let img_cache = Rc::new(RefCell::new(
            std::collections::HashMap::<(String, u32, u32), egui::TextureHandle>::new(),
        ));

        // Pointer: press ON a widget → forward the click to egui; press the body
        // (no widget there) → compositor drag (begin_move). Decided with last
        // frame's interactive rects, so a click never both drags and clicks.
        {
            let surf_press = Rc::clone(&surface);
            let surf_release = Rc::clone(&surface);
            let win_drag = win.clone();
            let click = GestureClick::new();
            click.connect_pressed(move |g, _n, x, y| {
                let pos = egui::pos2(x as f32, y as f32);
                let guard = surf_press.borrow();
                let on_widget = guard.as_ref().map_or(false, |s| s.wants_pointer_at(pos));
                if on_widget {
                    if let Some(s) = guard.as_ref() {
                        s.push_event(egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::default(),
                        });
                    }
                } else {
                    drop(guard);
                    begin_move(g, &win_drag, x, y);
                }
            });
            click.connect_released(move |_g, _n, x, y| {
                if let Some(s) = surf_release.borrow().as_ref() {
                    s.push_event(egui::Event::PointerButton {
                        pos: egui::pos2(x as f32, y as f32),
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    });
                }
            });
            win.add_controller(click);
        }

        // Pointer motion → egui hover.
        {
            let surf = Rc::clone(&surface);
            let motion = EventControllerMotion::new();
            motion.connect_motion(move |_m, x, y| {
                if let Some(s) = surf.borrow().as_ref() {
                    s.push_event(egui::Event::PointerMoved(egui::pos2(x as f32, y as f32)));
                }
            });
            win.add_controller(motion);
        }

        // Keyboard → egui (special keys as Key events, printable as Text). The
        // separate ESC controller below still closes the window.
        {
            let surf = Rc::clone(&surface);
            let ec = EventControllerKey::new();
            ec.connect_key_pressed(move |_e, key, _code, mods| {
                let guard = surf.borrow();
                let Some(s) = guard.as_ref() else {
                    return Propagation::Proceed;
                };
                let m = map_mods(mods);
                let special = match key {
                    gdk::Key::BackSpace => Some(egui::Key::Backspace),
                    gdk::Key::Return | gdk::Key::KP_Enter => Some(egui::Key::Enter),
                    gdk::Key::Tab => Some(egui::Key::Tab),
                    gdk::Key::Left => Some(egui::Key::ArrowLeft),
                    gdk::Key::Right => Some(egui::Key::ArrowRight),
                    gdk::Key::Up => Some(egui::Key::ArrowUp),
                    gdk::Key::Down => Some(egui::Key::ArrowDown),
                    gdk::Key::Delete => Some(egui::Key::Delete),
                    gdk::Key::Home => Some(egui::Key::Home),
                    gdk::Key::End => Some(egui::Key::End),
                    _ => None,
                };
                if let Some(ek) = special {
                    s.push_event(egui::Event::Key {
                        key: ek,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: m,
                    });
                } else if let Some(c) = key.to_unicode().filter(|c| !c.is_control()) {
                    s.push_event(egui::Event::Text(c.to_string()));
                }
                Propagation::Proceed
            });
            win.add_controller(ec);
        }

        // Tick: declare (app.view) → render (egui binder) → route Msgs back
        // (app.update). Lazy-init the GPU surface; skip when idle.
        {
            let surface = Rc::clone(&surface);
            let app = Rc::clone(&app);
            let scratch = Rc::clone(&scratch);
            let win_cb = win.clone();
            let last_alpha = RefCell::new(None::<Vec<(i32, i32, i32, i32)>>);
            win.add_tick_callback(move |_win, _clock| {
                let mut surf = surface.borrow_mut();
                if surf.is_none() {
                    *surf = Some(pollster::block_on(crate::gtk_egui::EguiSurface::new((cw, ch))));
                }
                if !surf.as_ref().unwrap().should_render() {
                    return ControlFlow::Continue;
                }
                let view = app.borrow().view();
                let out_msgs = Rc::new(RefCell::new(Vec::<ui::Msg>::new()));
                let out_msgs_c = Rc::clone(&out_msgs);
                let scratch_c = Rc::clone(&scratch);
                let img_cache_c = Rc::clone(&img_cache);
                let rgba = surf.as_mut().unwrap().render(|ctx, rects| {
                    *out_msgs_c.borrow_mut() = crate::egui_view::render_view(
                        ctx,
                        &view,
                        &mut scratch_c.borrow_mut(),
                        rects,
                        &mut img_cache_c.borrow_mut(),
                    );
                });
                drop(surf);
                for m in out_msgs.borrow_mut().drain(..) {
                    app.borrow_mut().update(m);
                }
                set_picture_bytes(&picture, &rgba, cw, ch);
                // Irregular click-through: capture input only where the pet's
                // rendered alpha is opaque; transparent areas pass through.
                let region = alpha_input_region(&rgba, cw, ch);
                apply_input_region(&win_cb, &region, &mut *last_alpha.borrow_mut());
                ControlFlow::Continue
            });
        }
    }
    #[cfg(not(feature = "egui"))]
    {
        let app = Rc::clone(app);
        let picture = picture.clone();
        let win_cb = win.clone(); // captured by the closure (receiver borrows `win`)
        let last_region = RefCell::new(None::<Vec<(i32, i32, i32, i32)>>);
        win.add_tick_callback(move |_win, _clock| {
            let mut a = app.borrow_mut();
            a.tick(Duration::from_secs_f32(1.0 / 60.0));
            let (w, h) = a.canvas_size();
            let region = a.input_region();
            let mut canvas = Canvas::new(w, h);
            a.render(&mut canvas);
            drop(a);
            set_picture(&picture, &canvas, w, h);
            apply_input_region(&win_cb, &region, &mut last_region.borrow_mut());
            ControlFlow::Continue
        });
    }

    // ESC to close (window is undecorated, so no titlebar close button)
    {
        let win_close = win.clone();
        let ec = EventControllerKey::new();
        ec.connect_key_pressed(move |_e, key, _code, _mods| {
            if key == gdk::Key::Escape {
                win_close.close();
            }
            Propagation::Proceed
        });
        win.add_controller(ec);
    }

    win.present();

    // Best-effort always-on-top via the Linux keep-above strategy.
    let mut pet_win = GtkPetWindow::new(win, (w, h), token);
    match pet_win.request_keep_above(KeepAboveMode::NativeLayer) {
        KeepAboveResult::Applied => {
            eprintln!("[geek-familiar] keep-above applied via {}", pet_win.strategy_id())
        }
        KeepAboveResult::Unsupported => {
            eprintln!(
                "[geek-familiar] keep-above ({}) unavailable — running without forced top",
                pet_win.strategy_id()
            )
        }
    }
}

fn paint_frame(app: &Rc<RefCell<Box<dyn App>>>, picture: &Picture) {
    let a = app.borrow();
    let (w, h) = a.canvas_size();
    let mut canvas = Canvas::new(w, h);
    a.render(&mut canvas);
    set_picture(picture, &canvas, w, h);
}

fn set_picture(picture: &Picture, canvas: &Canvas, w: u32, h: u32) {
    let bytes = Bytes::from(canvas.as_bytes());
    let tex = MemoryTexture::new(
        w as i32,
        h as i32,
        MemoryFormat::R8g8b8a8,
        &bytes,
        (w as usize) * 4,
    );
    picture.set_paintable(Some(&tex));
}

/// Upload a raw RGBA8 buffer to the `Picture` (the egui path produces one each
/// frame instead of a `Canvas`).
#[cfg(feature = "egui")]
fn set_picture_bytes(picture: &Picture, rgba: &[u8], w: u32, h: u32) {
    let bytes = Bytes::from(rgba);
    let tex = MemoryTexture::new(
        w as i32,
        h as i32,
        MemoryFormat::R8g8b8a8,
        &bytes,
        (w as usize) * 4,
    );
    picture.set_paintable(Some(&tex));
}

/// Hand an interactive window move to Mutter (xdg-toplevel move) from a GTK
/// gesture press. gtk4-rs 0.11 hides `ToplevelExt::begin_move`, so call the C
/// symbol directly (the GdkSurface→GdkToplevel cast is sound — same instance).
#[cfg(feature = "egui")]
fn begin_move(gesture: &GestureClick, win: &ApplicationWindow, x: f64, y: f64) {
    let Some(device) = gesture.current_event_device() else {
        return;
    };
    let Some(surface) = win.surface() else {
        return;
    };
    unsafe {
        let surface_ptr: *mut gtk4::gdk::ffi::GdkSurface = surface.to_glib_none().0;
        let device_ptr: *mut gtk4::gdk::ffi::GdkDevice = device.to_glib_none().0;
        gtk4::gdk::ffi::gdk_toplevel_begin_move(
            surface_ptr as *mut gtk4::gdk::ffi::GdkToplevel,
            device_ptr,
            gesture.current_button() as i32,
            x,
            y,
            gesture.current_event_time(),
        );
    }
}

/// Map GDK modifier flags → egui modifiers (for keyboard shortcuts in egui).
#[cfg(feature = "egui")]
fn map_mods(m: gdk::ModifierType) -> egui::Modifiers {
    egui::Modifiers {
        alt: m.contains(gdk::ModifierType::ALT_MASK),
        ctrl: m.contains(gdk::ModifierType::CONTROL_MASK),
        shift: m.contains(gdk::ModifierType::SHIFT_MASK),
        mac_cmd: false,
        command: m.contains(gdk::ModifierType::CONTROL_MASK),
    }
}

/// Build a click-capturing [`InputRegion`] from the rendered RGBA8 buffer's
/// alpha channel: per-row opaque spans become 1px-tall scanline rects. Only
/// where alpha > `THRESHOLD` captures input; transparent areas (alpha ≈ 0) pass
/// clicks through to the desktop → the window is an irregular shape matching
/// the pet's actual silhouette, not its rectangular bounding box.
#[cfg(feature = "egui")]
fn alpha_input_region(rgba: &[u8], w: u32, h: u32) -> InputRegion {
    const THRESHOLD: u8 = 16;
    let mut rects = Vec::new();
    for y in 0..h {
        let base = y as usize * w as usize * 4;
        let mut x = 0u32;
        while x < w {
            while x < w && rgba[base + x as usize * 4 + 3] <= THRESHOLD {
                x += 1;
            }
            if x >= w {
                break;
            }
            let x0 = x;
            while x < w && rgba[base + x as usize * 4 + 3] > THRESHOLD {
                x += 1;
            }
            rects.push(core::Rect::new(x0 as f32, y as f32, (x - x0) as f32, 1.0));
        }
    }
    InputRegion::from_rects(rects)
}

/// Build a cairo region from the input-region rects (empty slice → empty region
/// = full pass-through; else the union of rects = the click-capturing area).
fn build_cairo_region(region: &InputRegion) -> cairo::Region {
    let rects: Vec<cairo::RectangleInt> = region
        .rects
        .iter()
        .map(|r| {
            let (x, y, w, h) = r.to_pixels();
            cairo::RectangleInt::new(x, y, w, h)
        })
        .collect();
    cairo::Region::create_rectangles(&rects)
}

/// Set the surface's input region via raw FFI (gtk4-rs 0.11 doesn't bind
/// `gdk_surface_set_input_region`). `last` caches the previous pixel-rects so we
/// skip the per-frame FFI call when the pet hasn't moved a whole pixel.
/// Set the surface's input region via raw FFI (gtk4-rs 0.11 doesn't bind
/// `gdk_surface_set_input_region`). `last` caches the previous pixel-rects so we
/// skip the per-frame FFI call when the pet hasn't moved a whole pixel.
fn apply_input_region(
    win: &ApplicationWindow,
    region: &InputRegion,
    last: &mut Option<Vec<(i32, i32, i32, i32)>>,
) {
    let Some(surface) = win.surface() else {
        return; // surface not realized yet (first tick before present completes)
    };
    let pixels: Vec<(i32, i32, i32, i32)> =
        region.rects.iter().map(|r| r.to_pixels()).collect();
    if last.as_ref() == Some(&pixels) {
        return;
    }
    *last = Some(pixels);
    let cairo_region = build_cairo_region(region);
    // SAFETY: both handles are valid, thread-local GTK objects on the main thread.
    // GDK copies the region, so dropping `cairo_region` after is fine.
    unsafe {
        gtk4::gdk::ffi::gdk_surface_set_input_region(
            surface.to_glib_none().0,
            (&cairo_region).to_glib_none().0,
        );
    }
}

/// GTK4 realization of the cross-platform `PetWindow` interface.
pub struct GtkPetWindow {
    #[allow(dead_code)] // retained for M2 input-region/frame work
    win: ApplicationWindow,
    size: (u32, u32),
    token: String,
    keep_above: Box<dyn KeepAboveStrategy>,
}

impl GtkPetWindow {
    fn new(win: ApplicationWindow, size: (u32, u32), token: String) -> Self {
        Self {
            win,
            size,
            token,
            keep_above: crate::keep_above::detect(),
        }
    }
    fn strategy_id(&self) -> &'static str {
        self.keep_above.id()
    }
}

impl PetWindow for GtkPetWindow {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn set_input_region(&mut self, region: &InputRegion) {
        // Raw FFI (gtk4-rs 0.11 doesn't bind gdk_surface_set_input_region); see
        // apply_input_region above. The tick loop applies the live region each
        // frame; this trait method is the one-shot equivalent.
        if let Some(surface) = self.win.surface() {
            let cairo_region = build_cairo_region(region);
            unsafe {
                gtk4::gdk::ffi::gdk_surface_set_input_region(
                    surface.to_glib_none().0,
                    (&cairo_region).to_glib_none().0,
                );
            }
        }
    }

    fn request_keep_above(&mut self, mode: KeepAboveMode) -> KeepAboveResult {
        match mode {
            KeepAboveMode::Off => {
                self.keep_above.disable();
                KeepAboveResult::Applied
            }
            // detect() chose the strategy for this compositor; the GNOME path
            // identifies our window to the extension via the title token.
            _ => self.keep_above.enable(&crate::keep_above::PinRequest {
                app_id: APP_ID,
                token: &self.token,
            }),
        }
    }
}
