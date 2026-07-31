//! composer — the `Pipeline` (组装车间): wires Stage1Executor → Stage2Calibrator and emits
//! [`TurnEvent`]s to a caller-supplied callback. Pure orchestration — it does no printing, no
//! file I/O, no Stage3 logic.
//!
//! Stage2 calibration runs on its own `aura-stage2` worker thread so the Stage1 consume loop
//! never blocks on the LLM — streaming partials keep flowing while a previous utterance is
//! calibrated. An `Interim` for utterance N+1 can arrive BEFORE the `Final` for utterance N.

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

use audio_aura_asr::executor::{OnnxStage1Executor, Stage1Executor};
use audio_aura_asr::{Stage1Event, Utterance};

use crate::calibrator::Stage2Calibrator;
use crate::decision::Decision;

/// One turn surfaced to the caller.
#[derive(Debug)]
pub enum TurnEvent<'a> {
    Interim { seq: u64, partial: &'a str, at_s: f64 },
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

        // Stage2 worker on its own thread — drains finalized utterances off-thread
        let (tx, rx) = mpsc::channel::<Utterance>();
        {
            let on_turn = Arc::clone(&on_turn);
            let mut s2 = s2;
            thread::Builder::new()
                .name("aura-stage2".into())
                .spawn(move || {
                    for u in rx {
                        let t = Instant::now();
                        let d = s2.calibrate(&u);
                        let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                        on_turn(TurnEvent::Final { utterance: &u, decision: &d, route_ms });
                    }
                })
                .expect("spawn aura-stage2 worker");
        }

        // Stage1 consume loop (this thread) — partials pass through; finals are handed
        // to the worker so this loop never blocks on the LLM.
        s1.run(&mut move |ev| match ev {
            Stage1Event::Interim { seq, partial, at_s } => {
                on_turn(TurnEvent::Interim { seq, partial: &partial, at_s });
            }
            Stage1Event::Final(u) => {
                if tx.send(u).is_err() {
                    tracing::error!("stage2 worker gone — dropping utterance");
                }
            }
        });
    }
}
