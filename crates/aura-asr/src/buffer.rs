//! buffer.rs — audio ring buffer + noise gate. Ingest writes EVERYTHING (no gating); the consumer
//! drains frames at its own pace and optionally applies noise processing.
//!
//! Hardened (2026-07-14): the original `drain` used `VecDeque::drain(0..n)` which is O(remaining)
//! — every consumer frame memmoved the whole tail. Now `drain` is O(take) via `pop_front` (VecDeque
//! head pop is O(1), no shift). `push` trims with a single bounded drain so a stalled consumer
//! (e.g. a VAD that hangs) can't turn the ring into a 19 MB churn that drags the whole machine down.

use std::collections::VecDeque;

/// Append-only, draining audio ring. Capacity = 10 min mono 16 kHz: 9_600_000 samples ≈ 19.2 MB.
pub struct AudioRing {
    buf: VecDeque<i16>,
    cap: usize,
}

impl AudioRing {
    pub fn new(capacity_samples: usize) -> Self {
        AudioRing {
            buf: VecDeque::with_capacity(capacity_samples.min(8192).max(4096)),
            cap: capacity_samples,
        }
    }

    /// Append samples. If over capacity, the oldest samples are dropped in one bounded trim.
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples);
        if self.buf.len() > self.cap {
            let drop_n = self.buf.len() - self.cap;
            self.buf.drain(0..drop_n); // single O(drop_n) trim, only when overflowing
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn has_frame(&self, frame_samples: usize) -> bool {
        self.buf.len() >= frame_samples
    }

    /// Drain up to `n` samples from the front into a new Vec. O(take) — uses `pop_front` (VecDeque
    /// head pop is O(1), no memmove of the tail), unlike the old `drain(0..n)` which was O(remaining).
    pub fn drain(&mut self, n: usize) -> Vec<i16> {
        let take = n.min(self.buf.len());
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            // SAFETY: take <= len, so pop_front is Some each iteration.
            out.push(self.buf.pop_front().unwrap());
        }
        out
    }

    /// Fraction full (0.0–1.0). Lets a consumer apply backpressure (skip/flush) before the ring
    /// saturates if it can't keep up with realtime audio.
    pub fn fill_ratio(&self) -> f32 {
        self.buf.len() as f32 / self.cap as f32
    }
}

/// Zero out a frame if its RMS is below `floor` — run on the consumer side before VAD.
pub fn noise_gate(samples: &mut [i16], floor: f32) {
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_sq / samples.len().max(1) as f64).sqrt() as f32;
    if rms < floor {
        for s in samples.iter_mut() {
            *s = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ring_push_drain() {
        let mut r = AudioRing::new(100);
        r.push(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(r.drain(3), vec![1, 2, 3]);
        assert_eq!(r.len(), 3);
        assert_eq!(r.drain(100), vec![4, 5, 6]); // drain more than available → takes all
        assert_eq!(r.len(), 0);
    }
    #[test]
    fn ring_over_capacity_evicts_oldest() {
        let mut r = AudioRing::new(4);
        r.push(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(r.len(), 4);
        let all = r.drain(10);
        assert_eq!(all, vec![3, 4, 5, 6]); // oldest (1,2) evicted
    }
    #[test]
    fn ring_fill_ratio() {
        let mut r = AudioRing::new(10);
        assert_eq!(r.fill_ratio(), 0.0);
        r.push(&[0i16; 5]);
        assert!((r.fill_ratio() - 0.5).abs() < 1e-6);
    }
    #[test]
    fn noise_gate_silence() {
        let mut f = vec![1i16; 100];
        noise_gate(&mut f, 500.0);
        assert!(f.iter().all(|&s| s == 0));
    }
}
