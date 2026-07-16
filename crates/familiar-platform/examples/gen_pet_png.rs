//! Generate a high-res test "pet" PNG: a coral circle on a transparent
//! surround, with a 2px anti-aliased edge (a good crispness test for the
//! Lanczos downscale). Run:
//!   cargo run -p platform --example gen_pet_png --features egui -- /tmp/pet_test.png

fn main() {
    use image::{ImageBuffer, Rgba};
    let out = std::env::args().nth(1).unwrap_or_else(|| "/tmp/pet_test.png".into());
    let (w, h) = (1024u32, 1024u32);
    let (cx, cy, r) = (512.0f32, 512.0f32, 400.0f32);
    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            // 2px-wide smooth (anti-aliased) alpha edge around radius r
            let a = ((r - d).clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0;
            if d < r + 1.0 {
                img.get_pixel_mut(x, y).0 = [0xFF, 0x6F, 0x61, a as u8];
            }
            // transparent elsewhere (default [0,0,0,0])
        }
    }
    img.save(&out).expect("save png");
    println!("wrote {out} ({w}x{h}, coral circle r={r} on transparent)");
}
