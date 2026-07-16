//! composer — the `Pipeline` (组装车间): wires Stage1Executor → Stage2Calibrator and emits
//! [`TurnEvent`]s to a caller-supplied callback. Pure orchestration — it does no printing, no
//! file I/O, no Stage3 logic. The caller (the `stage12_live` bench example, or the `daemon`)
//! decides what to do with each turn (print / report / invoke a Stage3 trigger).
//!
//! Stage3 is NOT wired here: aura-core does not depend on aura-agent (the dependency points the
//! other way — aura-agent → aura-core). The daemon, which does depend on aura-agent, plugs a
//! Stage3 rule trigger into its own `on_turn` callback. This keeps the composer a pure S1→S2 pipe.

use std::time::Instant;

use audio_aura_asr::executor::{OnnxStage1Executor, Stage1Executor};
use audio_aura_asr::{Stage1Event, Utterance};
use audio_aura_router::calibrator::{RouterStage2Calibrator, Stage2Calibrator};
use audio_aura_router::Decision;

/// One turn surfaced to the caller. `Final` carries the calibrated decision + Stage2 latency.
#[derive(Debug)]
pub enum TurnEvent<'a> {
    /// A live streaming partial (pre-finalization).
    Interim { partial: &'a str, at_s: f64 },
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
    /// invoked for every streaming partial and every calibrated utterance.
    pub fn run<F: FnMut(TurnEvent)>(self, mut on_turn: F) -> ! {
        let mut s2 = self.s2; // move out so the closure can own + mutate it
        self.s1.run(&mut move |ev| match ev {
            Stage1Event::Interim { partial, at_s } => {
                on_turn(TurnEvent::Interim { partial: &partial, at_s });
            }
            Stage1Event::Final(u) => {
                let t = Instant::now();
                let d = s2.calibrate(&u);
                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                on_turn(TurnEvent::Final { utterance: &u, decision: &d, route_ms });
            }
        });
    }
}
