//! CPU RGBA8 framebuffer. The cross-platform "what the renderer paints" target:
//! renderers fill `buf`, platforms blit it to the window surface.

use crate::geometry::Color;

/// Row-major RGBA8 framebuffer, `width * height * 4` bytes.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub buf: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        let len = (width as usize) * (height as usize) * 4;
        Self {
            width,
            height,
            buf: vec![0; len],
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.buf.clear();
            self.buf.resize((width as usize) * (height as usize) * 4, 0);
        }
    }

    /// Fill the whole canvas with one color.
    pub fn clear(&mut self, c: Color) {
        for px in self.buf.chunks_exact_mut(4) {
            px[0] = c.r;
            px[1] = c.g;
            px[2] = c.b;
            px[3] = c.a;
        }
    }

    /// Blit a single pixel using straight-alpha "src-over" compositing.
    pub fn put(&mut self, x: i32, y: i32, c: Color) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return;
        }
        let i = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
        let dst = &mut self.buf[i..i + 4];
        let sa = c.a as u32;
        let da = dst[3] as u32;
        let out_a = sa + da * (255 - sa) / 255;
        if out_a == 0 {
            dst[0] = 0;
            dst[1] = 0;
            dst[2] = 0;
            dst[3] = 0;
            return;
        }
        let src_rgb = [c.r, c.g, c.b];
        for k in 0..3 {
            let sv = src_rgb[k] as u32 * sa;
            let dv = dst[k] as u32 * da * (255 - sa) / 255;
            dst[k] = ((sv + dv) / out_a) as u8;
        }
        dst[3] = out_a as u8;
    }

    /// RGBA bytes, row-major, top-to-bottom.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Write a binary PPM (P6, RGB — no alpha) for headless smoke-tests.
    /// PPM can't show transparency, so use an opaque background for tests.
    pub fn save_ppm(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        write!(f, "P6\n{} {}\n255\n", self.width, self.height)?;
        let mut rgb = Vec::with_capacity(self.buf.len() / 4 * 3);
        for px in self.buf.chunks_exact(4) {
            rgb.push(px[0]);
            rgb.push(px[1]);
            rgb.push(px[2]);
        }
        f.write_all(&rgb)?;
        Ok(())
    }
}
