//! Stage2Calibrator — 联合整流 (joint calibration): turns a window's [`VadSegment`] texts into
//! polished text (加标点 / 修同音错字 / 英文规范 / 专有名词修正). Wraps an
//! [`dp_models::LlmProvider`] (local mistral.rs or remote) + the LLM-layer hotword store
//! (shared with Stage3) + the user-correction ring (shared with POST /api/correct).
//!
//! 窗口状态机 (2026-08-17 边界范式,规格修订):内部只维护**当前窗口的最后一次联合整流
//! 结果**——每个 `Batch` 事件(每段一次)整体覆盖它;`WindowEdge` 到来时**不再调用 LLM**
//! (最后一个段的 Batch 已把全窗口整流完),直接取存档作为该 VadWindow 的纠偏字段并
//! 移动左边界(清空状态)。事件在单一 worker 线程上有序到达(Batch×N → WindowEdge),
//! 状态不可能失步。The old ContextWindow (disabled — 3B 复读) is deleted: cross-sentence
//! context enters through the joint input itself.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::{VadSegment, VadWindow, WindowId};

use crate::prompt::PromptBuilder;

/// Stage2's correction pass (纠偏/整流), driven by the two Stage1 events:
/// - `Stage1Event::Batch` → [`calibrate_window`](Self::calibrate_window) — joint calibration
///   of EVERY segment in the current window (multi-sentence); the result **overwrites** the
///   window's stored calibration;
/// - `Stage1Event::WindowEdge` → [`finalize_window`](Self::finalize_window) — move the left
///   boundary. **No LLM call**: the last Batch already calibrated the whole window; the
///   stored result simply becomes the VadWindow's calibrated field.
pub trait Stage2Calibrator: Send {
    /// Joint calibration (per Batch). Input = ALL segments so far; overwrites the current
    /// window's stored result.
    fn calibrate_window(&mut self, window_id: WindowId, segments: &[VadSegment]) -> String;
    /// Finalize (per WindowEdge): return the window's LAST joint calibration — no LLM run —
    /// and clear the window state (left boundary moves). Falls back to the window's
    /// best_text only in the impossible no-Batch case (defensive).
    fn finalize_window(&mut self, window: &VadWindow) -> String;
}

/// Stage2 turned off (`llm.backend: disable`): calibration is the identity — no LLM is
/// loaded, `calibrate_window` concatenates the segments' best texts, `finalize_window`
/// returns the window's best text. The `calibrated` field downstream (wire, archival)
/// carries the raw best text unchanged, so consumers see the same shapes with zero LLM
/// latency/cost. Useful for pure-ASR deployments and for A/B-ing Stage2's contribution.
pub struct PassThroughCalibrator;

impl Stage2Calibrator for PassThroughCalibrator {
    fn calibrate_window(&mut self, _window_id: WindowId, segments: &[VadSegment]) -> String {
        segments.iter().map(|s| s.best_text()).collect::<Vec<_>>().join("")
    }

    fn finalize_window(&mut self, window: &VadWindow) -> String {
        window.best_text().into_owned()
    }
}

/// Stage2 纠偏的输入源（配置 `llm.input`）——选择把哪些识别文本喂给 LLM。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LlmInput {
    /// 只用 batch 结果（`VadSegment::best_text()`：batch 优先，流式回退）。默认——batch 是权威。
    #[default]
    #[serde(rename = "batch")]
    Batch,
    /// 只用流式结果（`streaming_text`）——热词偏置更强、句首更全，但同音字更多。
    #[serde(rename = "stream")]
    Stream,
    /// batch + 流式双通道对照（`<primary_transcript>` + `<secondary_transcript>`）——批式丢句首
    /// 时由流式补回（见 [`crate::prompt::DUAL_TRANSCRIPT_INSTRUCTION`]）。
    #[serde(rename = "both")]
    Both,
}

/// Default Stage2 calibrator over an [`dp_models::LlmProvider`]. Reads the latest hotwords
/// (shared with Stage3) and user corrections on every call.
pub struct Stage2CalibratorImpl {
    llm: Arc<dyn dp_models::LlmProvider>,
    /// Shared with Stage3 — the feedback channel. Read fresh on every calibrate.
    hotwords: Arc<Mutex<Vec<String>>>,
    /// User corrections (raw→corrected pairs), shared with daemon's POST /api/correct handler.
    /// Read fresh on every calibrate — the correction feedback channel.
    corrections: Arc<Mutex<Vec<(String, String)>>>,
    /// 纠偏输入源（`llm.input`）。
    input: LlmInput,
    /// 窗口状态机的全部状态:当前窗口 id + 其最后一次联合整流结果(每个 Batch 覆盖,
    /// WindowEdge 消费并清空 = 移动左边界)。
    current: Option<(WindowId, String)>,
}

impl Stage2CalibratorImpl {
    /// `hotwords` is shared (clone the Arc from wherever Stage3 holds it); `llm` is the local
    /// `Calibrator` or a remote `HttpLlm` (as `Arc<dyn LlmProvider>`). `input` selects the
    /// calibration source text (batch / stream / both).
    pub fn new(
        llm: Arc<dyn dp_models::LlmProvider>,
        hotwords: Arc<Mutex<Vec<String>>>,
        corrections: Arc<Mutex<Vec<(String, String)>>>,
        input: LlmInput,
    ) -> Self {
        Self { llm, hotwords, corrections, input, current: None }
    }
}

impl Stage2Calibrator for Stage2CalibratorImpl {
    fn calibrate_window(&mut self, window_id: WindowId, segments: &[VadSegment]) -> String {
        let calibrated = match self.input {
            LlmInput::Batch => {
                // One line per segment — the joint input IS the cross-sentence context.
                let texts: Vec<&str> = segments.iter().map(|s| s.best_text()).collect();
                self.joint_calibrate(&texts, None)
            }
            LlmInput::Stream => {
                let texts: Vec<&str> = segments.iter().map(|s| s.streaming_text.as_str()).collect();
                self.joint_calibrate(&texts, None)
            }
            LlmInput::Both => {
                let texts: Vec<&str> = segments.iter().map(|s| s.best_text()).collect();
                let streaming = segments
                    .iter()
                    .map(|s| s.streaming_text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.joint_calibrate(&texts, Some(&streaming))
            }
        };
        // 识别一次之后,覆盖当前窗口的整流结果(WindowEdge 时它就是 VadWindow 的纠偏字段)。
        self.current = Some((window_id, calibrated.clone()));
        calibrated
    }

    fn finalize_window(&mut self, window: &VadWindow) -> String {
        // 移动左边界:取走存档(不匹配/无存档 = 防御路径,理论不可达——窗口必有 Batch)。
        match self.current.take() {
            Some((id, calibrated)) if id == window.id => calibrated,
            _ => {
                tracing::warn!(
                    window = window.id,
                    "WindowEdge 无匹配整流存档——回退窗口 best_text(理论不可达)"
                );
                window.best_text().into_owned()
            }
        }
    }
}

impl Stage2CalibratorImpl {
    /// The shared core: build the prompt (corrections → hotwords → joint text in the XML
    /// envelope), run the LLM, fall back to the raw text on failure. `streaming_ref` (Some for
    /// [`LlmInput::Both`]) adds the dual-transcript envelope so the LLM can补回 batch 丢的句首.
    fn joint_calibrate(&mut self, texts: &[&str], streaming_ref: Option<&str>) -> String {
        let hotwords = self.hotwords.lock().unwrap().clone();
        let corrections = self.corrections.lock().unwrap().clone();

        let mut pb = PromptBuilder::new_multi(texts).hotwords(&hotwords);
        // Dual-transcript (llm.input: both): streaming head/tail is fuller — the instruction
        // tells the LLM to补回 real words batch dropped at the segment head.
        if let Some(sref) = streaming_ref {
            pb = pb.streaming_ref(sref);
        }
        // User corrections (raw→corrected) — authoritative examples, highest priority.
        if !corrections.is_empty() {
            pb = pb.corrections(&corrections);
        }
        let (system, user) = pb.build();
        tracing::debug!(target: "stage2::prompt", system = %system, user = %user, "calibrate prompt");

        // LLM returns plain text — no JSON parsing. Failure → the raw text, unchanged.
        self.llm.complete(&system, &user).unwrap_or_else(|_| texts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SegmentId;

    /// Counting LLM stub — the shared handle lets the test READ the call count, proving
    /// finalize_window makes NO LLM call.
    struct CountingLlm(Arc<Mutex<usize>>);
    impl dp_models::LlmProvider for CountingLlm {
        fn complete(&self, _system: &str, _user: &str) -> anyhow::Result<String> {
            *self.0.lock().unwrap() += 1;
            Ok("整流OK".into())
        }
    }

    fn seg(id: SegmentId) -> VadSegment {
        VadSegment {
            id,
            audio_id: id,
            start_s: 0.0,
            end_s: 0.1,
            streaming_text: format!("流式{id}"),
            batch_text: Some(format!("段{id}")),
        }
    }

    fn window(id: WindowId) -> VadWindow {
        VadWindow {
            id,
            segments: vec![seg(1), seg(2)],
            start_s: 0.0,
            end_s: 1.0,
            streaming_text: "拼接".into(),
            batch_text: Some("窗口批式".into()),
            pcm: std::sync::Arc::new(Vec::new()),
        }
    }

    fn s2() -> (Stage2CalibratorImpl, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
        let s = Stage2CalibratorImpl::new(
            Arc::new(CountingLlm(Arc::clone(&calls))),
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
            LlmInput::Batch,
        );
        (s, calls)
    }

    /// Capturing LLM stub — records the USER prompt so the test can assert which source text
    /// (batch / stream / both) was fed.
    struct CapturingLlm(Arc<Mutex<Option<String>>>);
    impl dp_models::LlmProvider for CapturingLlm {
        fn complete(&self, _system: &str, user: &str) -> anyhow::Result<String> {
            *self.0.lock().unwrap() = Some(user.to_string());
            Ok("整流OK".into())
        }
    }

    fn s2_with_input(input: LlmInput) -> (Stage2CalibratorImpl, Arc<Mutex<Option<String>>>) {
        let user = Arc::new(Mutex::new(None));
        let s = Stage2CalibratorImpl::new(
            Arc::new(CapturingLlm(Arc::clone(&user))),
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
            input,
        );
        (s, user)
    }

    #[test]
    fn llm_input_selects_source_text() {
        let segs = vec![seg(1), seg(2)]; // batch "段1/段2", streaming "流式1/流式2"

        // batch（默认）：只喂 batch 文本，无 streaming 信封。
        let (mut s, user) = s2_with_input(LlmInput::Batch);
        s.calibrate_window(1, &segs);
        let u = user.lock().unwrap().clone().unwrap();
        assert!(u.contains("段1") && u.contains("段2"), "batch 文本进 prompt: {u}");
        assert!(!u.contains("流式1"), "batch 模式不喂流式");
        assert!(!u.contains("secondary_transcript"), "无双通道信封");

        // stream：只喂流式文本。
        let (mut s, user) = s2_with_input(LlmInput::Stream);
        s.calibrate_window(1, &segs);
        let u = user.lock().unwrap().clone().unwrap();
        assert!(u.contains("流式1") && u.contains("流式2"), "流式文本进 prompt: {u}");
        assert!(!u.contains("段1"), "stream 模式不喂 batch");

        // both：batch 进 primary_transcript + 流式进 secondary_transcript。
        let (mut s, user) = s2_with_input(LlmInput::Both);
        s.calibrate_window(1, &segs);
        let u = user.lock().unwrap().clone().unwrap();
        assert!(u.contains("段1") && u.contains("段2"), "both 模式 batch 进 primary_transcript");
        assert!(u.contains("流式1") && u.contains("流式2"), "both 模式流式进 secondary_transcript");
        assert!(u.contains("secondary_transcript"), "双通道信封存在");
    }

    #[test]
    fn window_state_machine_overwrites_and_finalizes_without_llm() {
        let (mut s, calls) = s2();
        // 两个 Batch(同窗口):每次联合整流跑一次 LLM,结果覆盖窗口存档。
        assert_eq!(s.calibrate_window(7, &[seg(1)]), "整流OK");
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(s.calibrate_window(7, &[seg(1), seg(2)]), "整流OK");
        assert_eq!(*calls.lock().unwrap(), 2, "每个 Batch 一次 LLM");
        // WindowEdge:不跑 LLM,直接返回存档(= 最后一次联合整流)。
        assert_eq!(s.finalize_window(&window(7)), "整流OK", "final = 最后一次 Batch 的整流结果");
        assert_eq!(*calls.lock().unwrap(), 2, "finalize 零 LLM 调用");
        // finalize 后状态清空(左边界已移):再 finalize 走防御回退 best_text,依然零 LLM。
        assert_eq!(s.finalize_window(&window(9)), "窗口批式", "无存档回退窗口 best_text");
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    #[test]
    fn shared_hotword_store_visible_to_both() {
        // The Stage3→Stage2 feedback channel: the same Arc<Mutex<Vec<String>>> is mutated by
        // Stage3 and read by Stage2. (Calibrator construction needs the real model, exercised
        // in the example; here we just prove the sharing primitive.)
        use std::sync::{Arc, Mutex};
        let store: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec!["Rust".into()]));
        let reader = Arc::clone(&store);
        store.lock().unwrap().push("Bevy".into()); // Stage3 adds
        assert_eq!(*reader.lock().unwrap(), vec!["Rust".to_string(), "Bevy".to_string()]);
    }
}
