//! wav — minimal PCM WAV (de)serialization. Mono/stereo 16-bit, any sample rate. Used to persist
//! utterances for inspection and to read test fixtures, without pulling in a wav crate.

use std::fs;
use std::io::Write;
use std::path::Path;

/// Encode mono 16-bit PCM as in-memory WAV bytes (44-byte header + data). For HTTP serving
/// without touching the filesystem.
pub fn wav_bytes(pcm: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2) };
    out.extend_from_slice(bytes);
    out
}

/// Write mono 16-bit PCM as a self-contained WAV (44-byte header + data). `sample_rate` e.g. 16000.
pub fn save_wav(path: &Path, pcm: &[i16], sample_rate: u32) -> std::io::Result<()> {
    let data_len = (pcm.len() * 2) as u32;
    let mut hdr = [0u8; 44];
    hdr[0..4].copy_from_slice(b"RIFF");
    hdr[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes());
    hdr[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    hdr[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    hdr[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    hdr[28..32].copy_from_slice(&(sample_rate * 2).to_le_bytes());
    hdr[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
    hdr[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    hdr[36..40].copy_from_slice(b"data");
    hdr[40..44].copy_from_slice(&data_len.to_le_bytes());
    let mut f = fs::File::create(path)?;
    f.write_all(&hdr)?;
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2) };
    f.write_all(bytes)?;
    Ok(())
}

/// Read a PCM WAV → (mono i16 samples, sample_rate). Downmixes stereo → mono.
pub fn read_wav_i16(path: &Path) -> std::io::Result<(Vec<i16>, u32)> {
    let b = fs::read(path)?;
    let fmt = b.windows(4).position(|w| w == b"fmt ").map(|p| p + 8).unwrap_or(12);
    let channels = u16::from_le_bytes([b[fmt + 2], b[fmt + 3]]);
    let sample_rate = u32::from_le_bytes([b[fmt + 4], b[fmt + 5], b[fmt + 6], b[fmt + 7]]);
    let data = b.windows(4).position(|w| w == b"data").map(|p| p + 8).unwrap_or(44);
    let mut pcm: Vec<i16> = b[data..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    if channels == 2 {
        pcm = pcm.chunks_exact(2).map(|c| ((c[0] as i32 + c[1] as i32) / 2) as i16).collect();
    }
    Ok((pcm, sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let tmp = std::env::temp_dir().join("audio_aura_asr_wav_test.wav");
        let pcm: Vec<i16> = (0..16000).map(|i| (i as i16).wrapping_mul(3)).collect();
        save_wav(&tmp, &pcm, 16000).unwrap();
        let (back, sr) = read_wav_i16(&tmp).unwrap();
        assert_eq!(sr, 16000);
        assert_eq!(back, pcm);
        let _ = std::fs::remove_file(&tmp);
    }
}
