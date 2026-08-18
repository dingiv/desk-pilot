//! composer — the `Pipeline` (组装车间): wires Stage1Executor → Stage2Calibrator and emits
//! [`TurnEvent`]s to a caller-supplied callback. Pure orchestration — it does no printing, no
//! file I/O, no Stage3 logic.
//!
//! Stage2 calibration runs on its own `aura-stage2` worker thread so the Stage1 consume loop
//! never blocks on the LLM — streaming partials keep flowing while a window is being
//! calibrated. An `Interim` for segment N+1 can arrive BEFORE the `WindowFinal` for window N.
//!
//! The worker drains the two Stage1 triggers off an mpsc channel (Interim never crosses the
//! channel — it passes straight through on the Stage1 thread): `Batch` →
//! [`Stage2Calibrator::calibrate_window`] (joint calibration of the current window, result
//! overwrites the window's stored calibration) → [`TurnEvent::WindowCalibrated`];
//! `WindowEdge` → [`Stage2Calibrator::finalize_window`] (NO LLM — attach the stored
//! calibration as the window's final field, move the left boundary) →
//! [`TurnEvent::WindowFinal`].

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

use audio_aura_asr::executor::{OnnxStage1Executor, Stage1Executor};
use audio_aura_asr::Stage1Event;

use crate::calibrator::Stage2Calibrator;

/// One turn surfaced to the caller.
#[derive(Debug)]
pub enum TurnEvent<'a> {
    /// Live streaming partial for the CURRENT segment (raw, uncalibrated). Straight from the
    /// Stage1 thread — NOT a Stage2 input (D2: no live-partial calibration).
    Interim { window_id: u64, segment_id: u64, partial: &'a str, at_s: f64 },
    /// Stage2's provisional JOINT calibration of the current window (per Batch) — the
    /// calibrated text so far, replacing the previous calibration of the same window.
    WindowCalibrated { window_id: u64, calibrated: String, route_ms: f64 },
    /// The settled window's final calibration (per WindowEdge) — the window's LAST joint
    /// calibration attached as its field (no extra LLM run). Window-granularity final (D3).
    WindowFinal { window: &'a audio_aura_asr::VadWindow, calibrated: String, route_ms: f64 },
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

        // Stage2 worker on its own thread — drains the two Stage1 triggers off-channel.
        // Events arrive in order on this single thread (Batch×N → WindowEdge), so Stage2's
        // tiny window state (last calibration per window) can never desync.
        let (tx, rx) = mpsc::channel::<Stage1Event>();
        {
            let on_turn = Arc::clone(&on_turn);
            let mut s2 = s2;
            thread::Builder::new()
                .name("aura-stage2".into())
                .spawn(move || {
                    for ev in rx {
                        match ev {
                            Stage1Event::Batch { window_id, segments } => {
                                let t = Instant::now();
                                let calibrated = s2.calibrate_window(window_id, &segments);
                                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                                on_turn(TurnEvent::WindowCalibrated {
                                    window_id,
                                    calibrated,
                                    route_ms,
                                });
                            }
                            Stage1Event::WindowEdge { window } => {
                                let t = Instant::now();
                                // 定稿不跑 LLM:取该窗口最后一次 Batch 联合整流的存档
                                // (最后一个段到来时整流已完成),移动左边界。
                                let calibrated = s2.finalize_window(&window);
                                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                                on_turn(TurnEvent::WindowFinal {
                                    window: &window,
                                    calibrated,
                                    route_ms,
                                });
                            }
                            Stage1Event::Interim { .. } => {
                                // Never sent down the channel (the Stage1 loop handles Interim
                                // inline) — defensive no-op if that ever changes.
                            }
                        }
                    }
                })
                .expect("spawn aura-stage2 worker");
        }

        // Stage1 consume loop (this thread) — Interim partials pass straight through; the two
        // Stage2 triggers are handed to the worker so this loop never blocks on the LLM.
        s1.run(&mut move |ev| match ev {
            Stage1Event::Interim { window_id, segment_id, partial, at_s } => {
                on_turn(TurnEvent::Interim {
                    window_id,
                    segment_id,
                    partial: &partial,
                    at_s,
                });
            }
            ev @ (Stage1Event::Batch { .. } | Stage1Event::WindowEdge { .. }) => {
                if tx.send(ev).is_err() {
                    tracing::error!("stage2 worker gone — dropping event");
                }
            }
        });
    }
}
