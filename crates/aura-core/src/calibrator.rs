//! Stage2Calibrator — 联合整流 (joint calibration): turns a paragraph's [`VadSentence`] texts into
//! polished text (加标点 / 修同音错字 / 英文规范 / 专有名词修正). Wraps an
//! [`dp_models::LlmProvider`] (local mistral.rs or remote) + the LLM-layer hotword store
//! (shared with Stage3) + the user-correction ring (shared with POST /api/correct).
//!
//! 无状态 (2026-08-30 batch 异步化后):每次调用都是纯函数式的——输入是"当前段落全部句的
//! 文本"(payload 即段落),内部**不存任何段落状态**。batch 异步后,末句的 batch 文本可能
//! 晚于最后一个 `Batch` 事件到达,旧"存最后一次整流、定稿零 LLM 取存档"的不变式不再
//! 成立;因此 `calibrate_paragraph`(每 `Batch` 一次,live 预览)与 `finalize_paragraph`
//! (每段落定稿一次,用全句 best_text——此时句级 batch 已由 pipeline 补齐)**各自独立跑
//! 一次 LLM**。The old ContextWindow (disabled — 3B 复读) is deleted: cross-sentence
//! context enters through the joint input itself.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::{VadSentence, VadParagraph, ParagraphId};

use crate::prompt::PromptBuilder;

/// Stage2's correction pass (纠偏/整流), driven by the Stage1 events. The calibrator is
/// STATELESS — each call takes the full paragraph payload and runs its own LLM pass:
/// - 每句 batch 完成(BS 到达,round17b)→ [`calibrate_paragraph`](Self::calibrate_paragraph)
///   — joint calibration of EVERY sentence in the current paragraph (multi-sentence) — the
///   live preview(架构需求:batch 完成 → 之后纠偏,先后明确);
/// - readiness finalization (pipeline, when all sentence batches + the re-run are in) →
///   [`finalize_paragraph`](Self::finalize_paragraph) — ONE LLM pass over the paragraph's final
///   best texts (the last sentence's batch may have arrived after its `Batch`, so the final pass
///   cannot reuse a stored calibration).
/// 门禁契约:两个方法都只在"流式或 batch 至少一路有非空文本"时才真正调用 LLM;
/// 双路全空(纯噪声 / ASR 断链回退后流式也空)→ 零 LLM,直接回空文本。避免在无内容时
/// 烧一次无意义的远程调用。
pub trait Stage2Calibrator: Send {
    /// Joint calibration (per Batch, live preview). Input = ALL sentences so far (best_text,
    /// streaming fallback). Stateless — no stored result. **Gated: no LLM when both streaming
    /// and batch are empty for every sentence.**
    fn calibrate_paragraph(&mut self, paragraph_id: ParagraphId, sentences: &[VadSentence]) -> String;
    /// Finalize (per settled paragraph, once): run the LLM over the paragraph's final best texts
    /// (the pipeline patches in every sentence's batch before calling this). Falls back to the
    /// raw joined text on LLM failure (see `joint_calibrate`). **Gated: no LLM when both
    /// streaming and batch are empty for every sentence.**
    fn finalize_paragraph(&mut self, paragraph: &VadParagraph) -> String;
}

/// Stage2 turned off (`llm.backend: disable`): calibration is the identity — no LLM is
/// loaded, `calibrate_paragraph` concatenates the sentences' best texts, `finalize_paragraph`
/// returns the paragraph's best text. The `calibrated` field downstream (wire, archival)
/// carries the raw best text unchanged, so consumers see the same shapes with zero LLM
/// latency/cost. Useful for pure-ASR deployments and for A/B-ing Stage2's contribution.
pub struct PassThroughCalibrator;

impl Stage2Calibrator for PassThroughCalibrator {
    fn calibrate_paragraph(&mut self, _paragraph_id: ParagraphId, sentences: &[VadSentence]) -> String {
        sentences.iter().map(|s| s.best_text()).collect::<Vec<_>>().join("")
    }

    fn finalize_paragraph(&mut self, paragraph: &VadParagraph) -> String {
        paragraph.best_text().into_owned()
    }
}

/// Stage2 纠偏的输入源（配置 `llm.input`）——选择把哪些识别文本喂给 LLM。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LlmInput {
    /// 只用 batch 结果（`VadSentence::best_text()`：batch 优先，流式回退）——batch 权威。
    #[serde(rename = "batch")]
    Batch,
    /// 只用流式结果（`streaming_text`）——热词偏置更强、句首更全，但同音字更多。
    #[serde(rename = "stream")]
    Stream,
    /// batch + 流式**双通道对照**（`<primary_transcript>` + `<secondary_transcript>`）——
    /// 批式丢句首时由流式补回，流式同音字多由批式压住（见
    /// [`crate::prompt::DUAL_TRANSCRIPT_INSTRUCTION`]）。**默认**(round17c:纠偏纠的
    /// 就是两路识别的结果 —— 参数必须都传,单路是降级模式)。
    #[default]
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
        Self { llm, hotwords, corrections, input }
    }
}

/// 门禁:段落(全部句)是否**有内容可整流**——至少一句的流式或 batch 有非空文本。
/// 全空(纯噪声 / batch 与流式双路都空,常见于 ASR 断链回退后流式也听不出)→ `false`。
/// 此时执行 LLM 只会烧一次调用换回空/噪声,毫无意义 —— 调用方应跳过 LLM。
fn has_recognized_text(sentences: &[VadSentence]) -> bool {
    sentences
        .iter()
        .any(|s| !s.streaming_text.trim().is_empty() || !s.batch_text.as_deref().unwrap_or("").trim().is_empty())
}

impl Stage2Calibrator for Stage2CalibratorImpl {
    fn calibrate_paragraph(&mut self, paragraph_id: ParagraphId, sentences: &[VadSentence]) -> String {
        let _ = paragraph_id; // stateless — the id is for the caller's bookkeeping
        // 门禁:流式与 batch 双路全空 → 无内容可整流,零 LLM,直接回空(不烧一次无意义调用)。
        if !has_recognized_text(sentences) {
            return String::new();
        }
        match self.input {
            LlmInput::Batch => {
                // One line per sentence — the joint input IS the cross-sentence context.
                let texts: Vec<&str> =
                    sentences.iter().map(|s| s.best_text()).filter(|t| !t.trim().is_empty()).collect();
                self.joint_calibrate(&texts, None)
            }
            LlmInput::Stream => {
                let texts: Vec<&str> = sentences
                    .iter()
                    .map(|s| s.streaming_text.as_str())
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                self.joint_calibrate(&texts, None)
            }
            LlmInput::Both => {
                let texts: Vec<&str> =
                    sentences.iter().map(|s| s.best_text()).filter(|t| !t.trim().is_empty()).collect();
                let streaming = sentences
                    .iter()
                    .map(|s| s.streaming_text.as_str())
                    .collect::<Vec<_>>()
                    .join("");
                self.joint_calibrate(&texts, Some(&streaming))
            }
        }
    }

    fn finalize_paragraph(&mut self, paragraph: &VadParagraph) -> String {
        // 门禁:流式与 batch 双路全空 → 无内容可整流,零 LLM,回原文(空)。
        if !has_recognized_text(&paragraph.sentences) {
            return String::new();
        }
        // 定稿整流:全句 best_text(句级 batch 已由 pipeline 补齐;缺失句回退流式)。
        // batch 异步化后末句 batch 可能晚于最后一个 Batch 到达,不能复用 live 整存档 ——
        // 定稿自己跑一次 LLM。
        match self.input {
            LlmInput::Batch => {
                let texts: Vec<&str> =
                    paragraph.sentences.iter().map(|s| s.best_text()).filter(|t| !t.trim().is_empty()).collect();
                self.joint_calibrate(&texts, None)
            }
            LlmInput::Stream => {
                let texts: Vec<&str> = paragraph
                    .sentences
                    .iter()
                    .map(|s| s.streaming_text.as_str())
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                self.joint_calibrate(&texts, None)
            }
            LlmInput::Both => {
                let texts: Vec<&str> = paragraph
                    .sentences
                    .iter()
                    .map(|s| s.best_text())
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                let streaming =
                    paragraph.sentences.iter().map(|s| s.streaming_text.as_str()).collect::<Vec<_>>().join("");
                self.joint_calibrate(&texts, Some(&streaming))
            }
        }
    }
}

impl Stage2CalibratorImpl {
    /// The shared core: build the prompt (corrections → hotwords → joint text in the XML
    /// envelope), run the LLM, fall back to the raw text on failure. `streaming_ref` (Some for
    /// [`LlmInput::Both`]) adds the dual-transcript envelope so the LLM can补回 batch 丢的句首.
    fn joint_calibrate(&mut self, texts: &[&str], streaming_ref: Option<&str>) -> String {
        // Defense-in-depth:入口门禁(has_recognized_text)已挡"双路全空";此处再按**所选
        // 输入源**挡一次(如 Stream 模式下流式全空 → 无输入)→ 零 LLM,回原文(空)。
        if texts.is_empty() {
            return String::new();
        }
        let hotwords = self.hotwords.lock().unwrap().clone();
        let corrections = self.corrections.lock().unwrap().clone();

        let mut pb = PromptBuilder::new_multi(texts).hotwords(&hotwords);
        // Dual-transcript (llm.input: both): streaming head/tail is fuller — the instruction
        // tells the LLM to补回 real words batch dropped at the sentence head.
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
        self.llm.complete(&system, &user).unwrap_or_else(|_| texts.join(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SentenceId;

    /// Counting LLM stub — the shared handle lets the test READ the call count, proving
    /// finalize_paragraph makes NO LLM call.
    struct CountingLlm(Arc<Mutex<usize>>);
    impl dp_models::LlmProvider for CountingLlm {
        fn complete(&self, _system: &str, _user: &str) -> anyhow::Result<String> {
            *self.0.lock().unwrap() += 1;
            Ok("整流OK".into())
        }
    }

    fn sentence(id: SentenceId) -> VadSentence {
        VadSentence {
            id,
            audio_id: id,
            start_s: 0.0,
            end_s: 0.1,
            streaming_text: format!("流式{id}"),
            batch_text: Some(format!("句{id}")),
        }
    }

    fn paragraph(id: ParagraphId) -> VadParagraph {
        VadParagraph {
            id,
            sentences: vec![sentence(1), sentence(2)],
            start_s: 0.0,
            end_s: 1.0,
            streaming_text: "拼接".into(),
            batch_text: Some("段落批式".into()),
            pcm: std::sync::Arc::new(Vec::new()),
            batch_asr_ms: 0,
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
        let sentences = vec![sentence(1), sentence(2)]; // batch "句1/句2", streaming "流式1/流式2"

        // batch（默认）：只喂 batch 文本，无 streaming 信封。
        let (mut s, user) = s2_with_input(LlmInput::Batch);
        s.calibrate_paragraph(1, &sentences);
        let u = user.lock().unwrap().clone().unwrap();
        assert!(u.contains("句1") && u.contains("句2"), "batch 文本进 prompt: {u}");
        assert!(!u.contains("流式1"), "batch 模式不喂流式");
        assert!(!u.contains("secondary_transcript"), "无双通道信封");

        // stream：只喂流式文本。
        let (mut s, user) = s2_with_input(LlmInput::Stream);
        s.calibrate_paragraph(1, &sentences);
        let u = user.lock().unwrap().clone().unwrap();
        assert!(u.contains("流式1") && u.contains("流式2"), "流式文本进 prompt: {u}");
        assert!(!u.contains("句1"), "stream 模式不喂 batch");

        // both：batch 进 primary_transcript + 流式进 secondary_transcript。
        let (mut s, user) = s2_with_input(LlmInput::Both);
        s.calibrate_paragraph(1, &sentences);
        let u = user.lock().unwrap().clone().unwrap();
        assert!(u.contains("句1") && u.contains("句2"), "both 模式 batch 进 primary_transcript");
        assert!(u.contains("流式1") && u.contains("流式2"), "both 模式流式进 secondary_transcript");
        assert!(u.contains("secondary_transcript"), "双通道信封存在");
    }

    #[test]
    fn stateless_calibrate_and_finalize_runs_llm_independently() {
        let (mut s, calls) = s2();
        // 两个 Batch(同段落):无状态——每次联合整流独立跑一次 LLM,不依赖/不覆盖存档。
        assert_eq!(s.calibrate_paragraph(7, &[sentence(1)]), "整流OK");
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(s.calibrate_paragraph(7, &[sentence(1), sentence(2)]), "整流OK");
        assert_eq!(*calls.lock().unwrap(), 2, "每个 Batch 一次 LLM");
        // 定稿:独立跑一次 LLM(batch 异步化后末句 batch 可能晚到,不能复用 live 整流存档)。
        assert_eq!(s.finalize_paragraph(&paragraph(7)), "整流OK", "final = 定稿那次 LLM 的结果");
        assert_eq!(*calls.lock().unwrap(), 3, "finalize 恰好一次 LLM");
        // 无状态:同段落再 finalize 行为一致(不依赖之前的任何状态)。
        assert_eq!(s.finalize_paragraph(&paragraph(7)), "整流OK");
        assert_eq!(*calls.lock().unwrap(), 4);
    }

    #[test]
    fn finalize_empty_paragraph_is_zero_llm() {
        let (mut s, calls) = s2();
        let mut p = paragraph(7);
        p.sentences.clear();
        assert_eq!(s.finalize_paragraph(&p), "", "全空段落零 LLM,回原文(空)");
        assert_eq!(*calls.lock().unwrap(), 0, "空段落不跑 LLM");
    }

    /// 一个"双路全空"的句:streaming 与 batch 都空(纯噪声 / ASR 断链回退后流式也听不出)。
    fn empty_sentence(id: SentenceId) -> VadSentence {
        VadSentence {
            id,
            audio_id: id,
            start_s: 0.0,
            end_s: 0.1,
            streaming_text: String::new(),
            batch_text: None,
        }
    }

    /// 门禁:双路全空 → live 整流与定稿整流都**零 LLM**(不烧无意义调用),直接回空。
    #[test]
    fn gate_no_llm_when_stream_and_batch_both_empty() {
        let (mut s, calls) = s2();
        let empty = vec![empty_sentence(1), empty_sentence(2)];
        // live:全空 → 零 LLM,回空。
        assert_eq!(s.calibrate_paragraph(7, &empty), "", "双路全空 live 零 LLM");
        assert_eq!(*calls.lock().unwrap(), 0, "live 不跑 LLM");
        // 定稿:全空段落 → 零 LLM,回空。
        let mut p = paragraph(7);
        p.sentences = empty.clone();
        p.batch_text = None;
        assert_eq!(s.finalize_paragraph(&p), "", "双路全空定稿零 LLM");
        assert_eq!(*calls.lock().unwrap(), 0, "定稿不跑 LLM");
    }

    /// 门禁放行:只要有**一路**有输出(仅 batch 或仅流式),就正常整流(跑 LLM)。
    #[test]
    fn gate_passes_when_either_stream_or_batch_has_output() {
        // 仅 batch 有输出(streaming 空)。
        let (mut s, calls) = s2();
        let mut only_batch = empty_sentence(1);
        only_batch.batch_text = Some("只有批式".into());
        assert_eq!(s.calibrate_paragraph(7, &[only_batch]), "整流OK", "仅 batch 有输出 → 整流");
        assert_eq!(*calls.lock().unwrap(), 1, "batch 有输出 → 跑 LLM");
        // 仅流式有输出(batch 空/None)。
        let (mut s, calls) = s2();
        let mut only_stream = empty_sentence(2);
        only_stream.streaming_text = "只有流式".into();
        assert_eq!(s.calibrate_paragraph(7, &[only_stream]), "整流OK", "仅流式有输出 → 整流");
        assert_eq!(*calls.lock().unwrap(), 1, "流式有输出 → 跑 LLM");
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
