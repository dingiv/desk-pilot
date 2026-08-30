//! audio_store — the dedicated PCM data manager (R2 的"录音真实数据由专门 store 模块管理").
//! Owns every clip by [`AudioId`]; pipeline entities ([`crate::VadSentence`]/[`crate::VadParagraph`])
//! hold ids and never clone PCM around.
//!
//! Lifecycle: pre-settle hot store. The executor inserts each finalized sentence's PCM at EOS
//! (as a shared `Arc<Vec<i16>>` — the async batch job and the store share the SAME allocation,
//! no copy); at paragraph settle it `concat`s the paragraph's clips (once — the resulting
//! `Arc<Vec<i16>>` lives on the [`crate::VadParagraph`] and is shared with the re-run job) and
//! `evict`s the per-sentence clips. Memory is bounded by `cap_samples` (oldest-first eviction),
//! so a stuck paragraph can never grow the process without limit. This is deliberately NOT the
//! persistent archive (`aura_core::AudioArchive` handles post-settle WAV flush + retention) —
//! nothing here ever touches disk.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::AudioId;

/// Default capacity: 10 min @ 16 kHz mono (~19 MB) — same order as the executor's audio ring.
pub const DEFAULT_CAP_SAMPLES: usize = 16_000 * 600;

/// An id → PCM hot store with a sample-count cap. Concrete struct (single implementation —
/// a trait here would be indirection without a second backend). Thread-safe: `&self` methods
/// only; the executor is the sole producer, the settle path the sole reader. Clips are stored
/// as `Arc<Vec<i16>>` so the async batch worker can share a clip without copying it.
pub struct AudioStore {
    inner: Mutex<Inner>,
    cap_samples: usize,
}

struct Inner {
    clips: BTreeMap<AudioId, Arc<Vec<i16>>>,
    next_id: AudioId,
    /// Total samples currently held (for the cap check).
    total: usize,
}

impl AudioStore {
    pub fn new(cap_samples: usize) -> Self {
        Self { inner: Mutex::new(Inner { clips: BTreeMap::new(), next_id: 0, total: 0 }), cap_samples }
    }

    /// Store a clip, return its id. Evicts oldest-first while over `cap_samples` — eviction
    /// targets the OLDEST id still held, which in the live pipeline is always a settled
    /// paragraph's leftover (normally already evicted explicitly), so this is a safety valve.
    /// The `Arc` is shared — the caller may keep its own clone (e.g. hand it to a batch job).
    pub fn insert(&self, pcm: Arc<Vec<i16>>) -> AudioId {
        let mut g = self.inner.lock().unwrap();
        let id = g.next_id;
        g.next_id += 1;
        g.total += pcm.len();
        g.clips.insert(id, pcm);
        while g.total > self.cap_samples {
            let (&oldest, pcm) = g.clips.iter().next().expect("total>cap implies non-empty");
            g.total -= pcm.len();
            g.clips.remove(&oldest);
        }
        id
    }

    /// Concatenate the clips for `ids` (paragraph settle). Missing ids (already evicted)
    /// contribute nothing — callers treat the result as the paragraph PCM regardless.
    pub fn concat(&self, ids: &[AudioId]) -> Vec<i16> {
        let g = self.inner.lock().unwrap();
        let mut out = Vec::with_capacity(g.total.min(self.cap_samples));
        for &id in ids {
            if let Some(pcm) = g.clips.get(&id) {
                out.extend_from_slice(pcm);
            }
        }
        out
    }

    /// Drop the clips for `ids` (after settle — the paragraph's `Arc<Vec<i16>>` is now the only
    /// remaining copy). Unknown ids are ignored.
    pub fn evict(&self, ids: &[AudioId]) {
        let mut g = self.inner.lock().unwrap();
        for &id in ids {
            if let Some(pcm) = g.clips.remove(&id) {
                g.total -= pcm.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(n: usize) -> Arc<Vec<i16>> {
        Arc::new(vec![1i16; n])
    }

    #[test]
    fn insert_returns_monotonic_ids_and_concat_joins_in_order() {
        let s = AudioStore::new(1_000_000);
        let a = s.insert(pcm(10));
        let b = s.insert(pcm(20));
        assert_eq!((a, b), (0, 1), "ids are monotonic from 0");
        let joined = s.concat(&[a, b]);
        assert_eq!(joined.len(), 30, "concat joins in the GIVEN id order");
    }

    #[test]
    fn evict_drops_clips() {
        let s = AudioStore::new(1_000_000);
        let a = s.insert(pcm(10));
        s.evict(&[a]);
        assert!(s.concat(&[a]).is_empty(), "evicted clip contributes nothing");
        s.evict(&[a]); // double evict is a no-op
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let s = AudioStore::new(25);
        let a = s.insert(pcm(10)); // total 10
        let b = s.insert(pcm(10)); // total 20
        let _ = s.insert(pcm(10)); // total 30 > 25 → evict a (oldest) → total 20
        assert!(s.concat(&[a]).is_empty(), "oldest clip evicted on overflow");
        assert_eq!(s.concat(&[b]).len(), 10, "newer clips survive");
    }

    #[test]
    fn insert_shares_allocation_with_caller() {
        // The async batch job hands the SAME Arc to the store — no PCM copy on insert.
        let s = AudioStore::new(1_000_000);
        let pcm = Arc::new(vec![7i16; 100]);
        let id = s.insert(Arc::clone(&pcm));
        assert_eq!(Arc::strong_count(&pcm), 2, "store and caller share one allocation");
        let joined = s.concat(&[id]);
        assert_eq!(joined, vec![7i16; 100]);
    }
}
