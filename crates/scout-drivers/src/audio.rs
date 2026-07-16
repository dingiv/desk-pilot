//! [`AudioSource`] — the audio-capture (microphone) half, parallel to
//! [`crate::CaptureSource`] (screen).
//!
//! Where frames are pulled on demand, audio is a **continuous stream**: while a
//! client is listening the source must deliver every buffer gap-free at full
//! rate. Speech recognition cannot tolerate dropped samples the way video can
//! tolerate a lower frame rate, so there is no "lower the fps" escape hatch
//! here. The only sanctioned laziness is to pause capture entirely when
//! *nobody* is listening (see [`AudioSource::set_active`]).
//!
//! Delivery is push/broadcast: a backend captures once into an `Arc<[u8]>` chunk
//! and fans it out to every [`AudioSubscription`] (one bounded mpsc per client).
//! [`AudioSubscription`] is backend-agnostic — each backend's `subscribe()`
//! registers its sender and hands back a guard carrying an `unsub` closure that
//! removes it on drop.

use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};

use crate::error::Result;

/// Negotiated raw-PCM format.
///
/// We always negotiate interleaved **S16LE** little-endian samples (the de-facto
/// ASR wire format); `rate` / `channels` are what the backend actually fixated.
/// The PipeWire backend requests 16 kHz mono and lets PipeWire's converter
/// resample the mic, so in practice these are `16000` / `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    /// Sample rate in Hz (e.g. `16000`).
    pub rate: u32,
    /// Channel count (e.g. `1` for mono).
    pub channels: u8,
}

/// Backend-agnostic audio-source surface. A source captures continuously and
/// fans each chunk out to its subscribers; it can be paused when subscriber-less.
pub trait AudioSource {
    /// The negotiated PCM format, once the stream has fixated (`None` before the
    /// first buffer).
    fn format(&self) -> Option<AudioFormat>;

    /// Pause (`false`) / resume (`true`) the underlying capture. Pausing when no
    /// client is listening is the only way to reduce audio cost — a paused source
    /// stops the producer and costs ~zero CPU.
    fn set_active(&self, active: bool);

    /// Register a subscriber receiving every captured chunk. Returns a guard
    /// whose drop unsubscribes. Implementations fan one captured chunk (shared via
    /// `Arc`) to every live subscriber; a chronically slow client's chunks are
    /// skipped rather than blocking the capture thread.
    fn subscribe(&self) -> Result<AudioSubscription>;

    /// How many clients are currently subscribed (for idle detection: pause the
    /// source when this hits 0).
    fn subscriber_count(&self) -> usize;
}

/// A live subscription to an [`AudioSource`]'s chunk fan-out. Owns its receiver;
/// dropping it runs the backend-supplied `unsub` closure to remove the sender
/// from the fan-out.
pub struct AudioSubscription {
    rx: Receiver<std::sync::Arc<[u8]>>,
    unsub: Option<Box<dyn FnOnce() + Send>>,
}

impl AudioSubscription {
    /// Build a subscription from its receiver + an unsubscribe closure (the
    /// closure removes this subscriber's sender from the backend's fan-out).
    pub fn new(
        rx: Receiver<std::sync::Arc<[u8]>>,
        unsub: Box<dyn FnOnce() + Send>,
    ) -> Self {
        Self {
            rx,
            unsub: Some(unsub),
        }
    }

    /// Receive one chunk, blocking up to `timeout`. Returns the raw PCM bytes
    /// (S16LE interleaved samples) or an error on timeout / disconnect.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> std::result::Result<std::sync::Arc<[u8]>, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Non-blocking receive.
    pub fn try_recv(&self) -> std::result::Result<std::sync::Arc<[u8]>, TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for AudioSubscription {
    fn drop(&mut self) {
        if let Some(unsub) = self.unsub.take() {
            unsub();
        }
    }
}
