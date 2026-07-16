//! [`Frame`] — the canonical captured image. Stored as raw 32-bit **BGRA** pixels
//! because that is the native layout of the backends we target (PipeWire BGRx/BGRA,
//! DXGI Desktop Duplication, v4l2 capture cards). PNG/RGBA encoding is a one-off
//! helper on top, not the stored form.

use crate::error::{DriverError, Result};

/// One captured screen frame.
///
/// Pixel `(x, y)` lives at byte offset `(y * width + x) * 4`, channel order
/// **B, G, R, A** (blue first). `width * height * 4 == bgra.len()`, enforced by
/// [`Frame::new`].
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Frame {
    /// Construct from an existing BGRA buffer, validating the length matches the
    /// declared dimensions. Cheap when the buffer is already the right size.
    pub fn new(width: u32, height: u32, bgra: Vec<u8>) -> Result<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| DriverError::InvalidArg("frame dimensions overflow".into()))?;
        if bgra.len() != expected {
            return Err(DriverError::InvalidArg(format!(
                "bgra buffer is {} bytes, expected {} ({}x{}x4)",
                bgra.len(),
                expected,
                width,
                height
            )));
        }
        Ok(Self {
            width,
            height,
            bgra,
        })
    }

    /// A solid-color frame of the given size — useful for tests and placeholders.
    #[must_use]
    pub fn solid(width: u32, height: u32, b: u8, g: u8, r: u8) -> Self {
        let pixels = width as usize * height as usize;
        let mut bgra = Vec::with_capacity(pixels * 4);
        for _ in 0..pixels {
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
        Self {
            width,
            height,
            bgra,
        }
    }

    /// Encode this frame to PNG, swizzling BGRA→RGBA for the encoder. Pure Rust
    /// (`png` crate), infallible for a well-formed `Frame` (the only failure mode
    /// is OOM, which aborts).
    #[must_use]
    pub fn to_png(&self) -> Vec<u8> {
        // BGRA (B,G,R,A) -> RGBA (R,G,B,A).
        let mut rgba = Vec::with_capacity(self.bgra.len());
        for c in self.bgra.chunks_exact(4) {
            // chunks_exact(4) guarantees indices 0..3.
            rgba.extend_from_slice(&[c[2], c[1], c[0], c[3]]);
        }

        let mut out = Vec::with_capacity(self.bgra.len());
        let mut enc = png::Encoder::new(&mut out, self.width, self.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .expect("png header encode failed on a valid Frame");
        writer
            .write_image_data(&rgba)
            .expect("png data encode failed on a valid Frame");
        writer
            .finish()
            .expect("png finish failed on a valid Frame");
        out
    }

    /// Byte length of one row (handy for backends computing strides).
    #[must_use]
    pub fn row_bytes(&self) -> usize {
        self.width as usize * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_frame_has_correct_layout() {
        let f = Frame::solid(2, 3, 0x11, 0x22, 0x33); // BGR = 11,22,33
        assert_eq!(f.width, 2);
        assert_eq!(f.height, 3);
        assert_eq!(f.bgra.len(), 2 * 3 * 4);
        assert_eq!(f.row_bytes(), 8);
        // every pixel is B=11 G=22 R=33 A=ff
        for c in f.bgra.chunks_exact(4) {
            assert_eq!(c, &[0x11, 0x22, 0x33, 0xFF]);
        }
    }

    #[test]
    fn new_validates_length() {
        assert!(Frame::new(2, 2, vec![0u8; 16]).is_ok()); // exactly 2*2*4
        assert!(Frame::new(2, 2, vec![0u8; 15]).is_err()); // too short
        assert!(Frame::new(2, 2, vec![0u8; 17]).is_err()); // too long
    }

    #[test]
    fn new_rejects_dimension_overflow() {
        // near usize::max: width*height*4 must overflow.
        let huge = u32::MAX;
        assert!(Frame::new(huge, huge, Vec::new()).is_err());
    }

    #[test]
    fn to_png_roundtrips_dimensions_and_is_valid_png() {
        let f = Frame::solid(3, 2, 0x00, 0x00, 0xFF); // blue-in-BGR => red pixel (R=FF)
        let png_bytes = f.to_png();
        // PNG signature.
        assert_eq!(&png_bytes[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

        // Decode back and confirm size + that the pixel is red (RGBA FF 00 00 FF).
        let mut decoder = png::Decoder::new(&png_bytes[..]);
        decoder.set_transformations(png::Transformations::IDENTITY);
        let mut reader = decoder.read_info().expect("decode png");
        let info = reader.info();
        assert_eq!(info.width, 3);
        assert_eq!(info.height, 2);
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let _ = reader.next_frame(&mut buf).expect("read png frame");
        // first pixel RGBA: R=FF G=00 B=00 A=FF
        assert_eq!(&buf[..4], &[0xFF, 0x00, 0x00, 0xFF]);
    }
}
