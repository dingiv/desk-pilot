//! composer — the `Pipeline` (组装车间): wires Stage1Executor → Stage2Calibrator and emits
//! [`TurnEvent`]s to a caller-supplied callback. Pure orchestration — it does no printing, no
//! file I/O, no Stage3 logic. The caller (the `stage12_live` bench example, or the `daemon`)
//! decides what to do with each turn (print / report / invoke a Stage3 trigger).
//!
//! **Threading**: Stage2 calibration (~1-2s per utterance on the local Qwen router) runs on its
//! own `aura-stage2` worker thread, NOT on the Stage1 consume loop. The consume loop therefore
//! never blocks on the LLM — streaming partials keep flowing while a previous utterance is still
//! being calibrated. Consequence: an `Interim` for utterance N+1 can arrive BEFORE the `Final`
//! for utterance N; consumers must group events by `seq`, not by arrival order. `on_turn` is
//! invoked from both threads, hence `Fn + Send + Sync` (state goes in `Arc`/atomics, as the
//! daemon already does).
//!
//! Stage3 is NOT wired here: aura-core does not depend on aura-agent (the dependency points the
//! other way — aura-agent → aura-core). The daemon, which does depend on aura-agent, plugs a
//! Stage3 rule trigger into its own `on_turn` callback. This keeps the composer a pure S1→S2 pipe.

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

use audio_aura_asr::executor::{OnnxStage1Executor, Stage1Executor};
use audio_aura_asr::{Stage1Event, Utterance};
use audio_aura_router::calibrator::{RouterStage2Calibrator, Stage2Calibrator};
use audio_aura_router::Decision;

/// One turn surfaced to the caller. `Final` carries the calibrated decision + Stage2 latency.
#[derive(Debug)]
pub enum TurnEvent<'a> {
    /// A live streaming partial (pre-finalization). `seq` is the in-progress utterance's
    /// prospective sequence number — group partials with their utterance by it (arrival order is
    /// NOT a grouping key: with Stage2 on its own thread, Interim(N+1) may precede Final(N)).
    Interim { seq: u64, partial: &'a str, at_s: f64 },
    /// A finalized utterance + its calibration + how long Stage2 took.
    Final { utterance: &'a Utterance, decision: &'a Decision, route_ms: f64 },
}

/// The Stage1→Stage2 pipeline. Build with [`Pipeline::new`], then [`Pipeline::run`] blocks
/// forever driving the loop and invoking `on_turn` for each event.
pub struct Pipeline {
    s1: OnnxStage1Executor,
    s2: RouterStage2Calibrator,
}

impl Pipeline {
    pub fn new(s1: OnnxStage1Executor, s2: RouterStage2Calibrator) -> Self {
        Self { s1, s2 }
    }

    /// Run the pipeline. Blocks forever (the Stage1 consume loop never returns). `on_turn` is
    /// invoked for every streaming partial (from the Stage1 thread) and every calibrated
    /// utterance (from the `aura-stage2` worker thread) — see the module docs for the ordering
    /// contract.
    pub fn run<F>(self, on_turn: F) -> !
    where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        let Pipeline { s1, s2 } = self;
        let on_turn = Arc::new(on_turn);

        // ── Stage2 worker: owns the calibrator, drains finalized utterances off-thread ──
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

        // ── Stage1 consume loop (this thread): partials pass straight through; finals are
        //    handed to the worker so this loop never blocks on the LLM. ──
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
