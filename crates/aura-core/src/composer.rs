//! composer — the `Pipeline` (组装车间): wires Stage1Executor → Stage2Calibrator and emits
//! [`TurnEvent`]s to a caller-supplied callback. Pure orchestration — it does no printing, no
//! file I/O, no Stage3 logic.
//!
//! Stage2 calibration runs on its own `aura-stage2` worker thread so the Stage1 consume loop
//! never blocks on the LLM — streaming partials keep flowing while a previous utterance is
//! calibrated. An `Interim` for utterance N+1 can arrive BEFORE the `Final` for utterance N.
//!
//! Stage2 listens for the two [`Stage1Action`]s Stage1 emits. Both are calibrated on the worker:
//! `Batch` (普通 batch, pause ≥ min_silence ≈ 1s — per absorbed fragment) via
//! [`Stage2Calibrator::calibrate_provisional`] (does NOT commit to the ContextWindow, so a
//! growing utterance's intermediate states don't pollute it) → [`TurnEvent::CalibratedInterim`];
//! `MergeBatch` (大 MergeBatch, pause ≥ merge_gap ≈ 5s — the settled merged paragraph) via
//! [`Stage2Calibrator::calibrate`] → [`TurnEvent::Final`].

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

use audio_aura_asr::executor::{OnnxStage1Executor, Stage1Executor};
use audio_aura_asr::{Stage1Action, Stage1Event, Utterance};

use crate::calibrator::Stage2Calibrator;
use crate::decision::Decision;

/// One turn surfaced to the caller.
#[derive(Debug)]
pub enum TurnEvent<'a> {
    Interim { seq: u64, partial: &'a str, at_s: f64 },
    /// Stage2's provisional calibration of an in-progress utterance (from a `Batch` action) —
    /// the calibrated text so far, updating in place (same seq). Not yet settled.
    CalibratedInterim { seq: u64, calibrated: String, route_ms: f64 },
    Final { utterance: &'a Utterance, decision: &'a Decision, route_ms: f64 },
}

pub struct Pipeline {
    s1: OnnxStage1Executor,
    s2: Box<dyn Stage2Calibrator>,
}

impl Pipeline {
    pub fn new(s1: OnnxStage1Executor, s2: Box<dyn Stage2Calibrator>) -> Self {
        Self { s1, s2 }
    }

    /// Run the pipeline. Blocks forever (Stage1 consume loop never returns).
    pub fn run<F>(self, on_turn: F) -> !
    where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        let Pipeline { s1, s2 } = self;
        let on_turn = Arc::new(on_turn);

        // Stage2 worker on its own thread — drains the two Stage1Action kinds off-thread.
        // Batch actions (provisional) are calibrated WITHOUT committing to the ContextWindow;
        // only the settled MergeBatch commits, so the window holds one entry per utterance (the
        // final calibration), not the whole chain of intermediate states.
        let (tx, rx) = mpsc::channel::<Stage1Action>();
        {
            let on_turn = Arc::clone(&on_turn);
            let mut s2 = s2;
            thread::Builder::new()
                .name("aura-stage2".into())
                .spawn(move || {
                    for job in rx {
                        match job {
                            Stage1Action::Batch(u) => {
                                let t = Instant::now();
                                let d = s2.calibrate_provisional(&u);
                                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                                on_turn(TurnEvent::CalibratedInterim {
                                    seq: u.seq,
                                    calibrated: d.calibrated_text,
                                    route_ms,
                                });
                            }
                            Stage1Action::MergeBatch(u) => {
                                let t = Instant::now();
                                let d = s2.calibrate(&u);
                                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                                on_turn(TurnEvent::Final {
                                    utterance: &u,
                                    decision: &d,
                                    route_ms,
                                });
                            }
                        }
                    }
                })
                .expect("spawn aura-stage2 worker");
        }

        // Stage1 consume loop (this thread) — streaming partials pass straight through; the two
        // batch actions (Batch / MergeBatch) are handed to the worker so this loop never blocks
        // on the LLM.
        s1.run(&mut move |ev| match ev {
            Stage1Event::Interim { seq, partial, at_s } => {
                on_turn(TurnEvent::Interim { seq, partial: &partial, at_s });
            }
            Stage1Event::Action(a) => {
                if tx.send(a).is_err() {
                    tracing::error!("stage2 worker gone — dropping action");
                }
            }
        });
    }
}
