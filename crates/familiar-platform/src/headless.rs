//! Headless backend: renders exactly one frame to a PPM file and exits.
//!
//! Needs no windowing or graphics stack — verifies the render pipeline in CI
//! and on this container before GTK4 is installed.

use std::time::Duration;

use crate::{App, PlatformBackend};
use core::Canvas;

pub struct HeadlessBackend {
    pub out_path: std::path::PathBuf,
}

impl Default for HeadlessBackend {
    fn default() -> Self {
        Self {
            out_path: std::path::PathBuf::from("geek_familiar.ppm"),
        }
    }
}

impl HeadlessBackend {
    pub fn new(out_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            out_path: out_path.into(),
        }
    }
}

impl PlatformBackend for HeadlessBackend {
    fn run(&mut self, mut app: Box<dyn App>) -> ! {
        let (w, h) = app.canvas_size();
        app.handle_event(&crate::PlatformEvent::Resize { width: w, height: h });
        app.tick(Duration::from_secs_f32(1.0 / 60.0));
        let mut canvas = Canvas::new(w, h);
        app.render(&mut canvas);
        match canvas.save_ppm(&self.out_path) {
            Ok(()) => println!(
                "geek-familiar headless: wrote {} ({}x{})",
                self.out_path.display(),
                w,
                h
            ),
            Err(e) => eprintln!("headless render failed: {e}"),
        }
        std::process::exit(0)
    }
}
