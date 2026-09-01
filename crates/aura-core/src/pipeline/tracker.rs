//! tracker — 纯边界数学(分句归属 + 段落定稿决策,零 I/O,可单测):
//! 哪句属于哪段(id = 时间戳,严格递增)、merge_gap 何时关段、起音即开段、空段 GC。
//! 识别侧(流式会话、batch、AudioStore)都在 recognizer;这里只有 SOS/EOS 之上的
//! 段落划分决策。

use crate::pipeline::types::SettledParagraph;
use crate::{ParagraphId, SentenceId, VadSentence};
// ── Paragraph tracker: pure paragraphing decisions over wall-clock SOS/EOS (unit-testable, no I/O) ──
// The recognizer owns the ASR side (sessions, batch passes, the AudioStore); this tracker owns
// ONLY the boundary math — which sentence belongs to which paragraph, and when a paragraph closes.

/// The open paragraph: its settled sentences + whether a sentence is in progress (SOS seen,
/// EOS pending). The in-progress sentence's id/timing live recognizer-side(消费循环 + 流式
/// 任务);
/// the tracker only needs "is one active" for settle suppression. `opened_at` = 起音开段时刻
/// (VAD rising edge),供空段落 GC(起音后从未出句的微弱音频,静默满 merge_gap 即弃)。
struct OpenParagraph {
    paragraph_id: ParagraphId,
    sentences: Vec<VadSentence>,
    active: bool,
    opened_at: f64,
}

pub(crate) struct ParagraphTracker {
    merge_gap_s: f64,
    next_sentence_id: SentenceId,
    /// 最近分配的 paragraph id(供 `prospective` 给未开段落预生成;下一个随机 id)。
    last_win_id: ParagraphId,
    open: Option<OpenParagraph>,
}

impl ParagraphTracker {
    pub(crate) fn new(merge_gap_s: f64) -> Self {
        Self {
            merge_gap_s,
            next_sentence_id: 1,
            last_win_id: 0,
            open: None,
        }
    }

    /// 生成段落 id = **创建时刻时间戳**(UNIX_EPOCH 起微秒,u64)。严格递增:
    /// `max(now, last+1)` 防时钟回拨/同微秒碰撞;恒 ≠ 0。
    ///
    /// **id 即顺序(契约)**:客户端 Transcript 以 id 排序(BTreeMap 降序 = 说话顺序),
    /// 时间戳天然单调,取代旧随机器(`next_random_win_id` —— 随机 id 打碎了客户端
    /// 排序假设,§7-A);时间戳还让 id 在日志里可直接读出段落创建时刻。
    fn next_win_id(&mut self) -> ParagraphId {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let mut id = now.max(self.last_win_id.saturating_add(1));
        if id == 0 {
            id = 1;
        }
        self.last_win_id = id;
        id
    }

    /// VAD 起音(detected() false→true 翻转)即开段 —— **真键前置**(§7-B 幽灵段根治):
    /// 段落 id 在说话第一刻就分配,live partial 从第一条起携带真实段键;不再依赖
    /// 回溯 SOS(EOS 时刻)补开。已有开段(段内第 2+ 句)则不动。
    pub(crate) fn on_speech_onset(&mut self, now: f64) {
        if self.open.is_none() {
            let id = self.next_win_id();
            self.open = Some(OpenParagraph {
                paragraph_id: id,
                sentences: Vec::new(),
                active: false,
                opened_at: now,
            });
        }
    }

    /// VAD StartOfSpeech. NOTE: the SOS is RETROACTIVE — it fires at the sentence's EOS instant
    /// (its wall-clock IS the EOS time, NOT the speech onset), so the merge/split decision
    /// CANNOT happen here (using the EOS instant as the onset would inflate every gap by the
    /// sentence's own duration and settle on EVERY sentence — the "paragraph never has >1 sentence"
    /// bug). Normally the paragraph was already opened at the speech onset
    /// ([`Self::on_speech_onset`]); the open here is only a degenerate fallback (no rising edge
    /// was ever seen). This allocates the sentence id + marks the sentence active; the settle
    /// decision moves to [`Self::on_eos`], which uses the onset WALL-CLOCK recorded at the
    /// rising edge (round26:同一把量尺;end−PCM 反推偏晚 ~0.5s,曾致"同句中途换段")。
    pub(crate) fn on_sos(&mut self, now: f64) -> SentenceId {
        if self.open.is_none() {
            let id = self.next_win_id();
            self.open = Some(OpenParagraph {
                paragraph_id: id,
                sentences: Vec::new(),
                active: false,
                opened_at: now,
            });
        }
        let sentence_id = self.next_sentence_id;
        self.next_sentence_id += 1;
        self.open.as_mut().expect("paragraph just ensured").active = true;
        sentence_id
    }

    /// Settle the open paragraph iff the gap from `onset` (the NEXT sentence's true speech start)
    /// back to its last sentence ≥ merge_gap. `onset` must be the back-derived start, not the
    /// retroactive SOS instant.
    fn settle_if_gap(&mut self, onset: f64) -> Option<SettledParagraph> {
        let gap = {
            let w = self.open.as_ref()?;
            let last = w.sentences.last()?;
            onset - last.end_s
        };
        if gap >= self.merge_gap_s {
            self.take_open()
        } else {
            None
        }
    }

    /// Record a completed sentence. Settles the open paragraph FIRST when the gap since its last
    /// sentence ≥ merge_gap (using `sentence.start_s` = rising-edge onset wall-clock), then pushes this
    /// sentence into the (possibly fresh) paragraph. Returns (settled spans, paragraph id, ALL sentences
    /// so far) — the payload IS the paragraph, so Stage2 stays stateless.
    pub(crate) fn on_eos(
        &mut self,
        sentence: VadSentence,
    ) -> (Option<SettledParagraph>, ParagraphId, Vec<VadSentence>) {
        let settled = self.settle_if_gap(sentence.start_s);
        if self.open.is_none() {
            // First sentence, or the previous paragraph just settled. opened_at 用回溯
            // onset(正常路径在起音已开段,这里是防御兜底)。
            let id = self.next_win_id();
            self.open = Some(OpenParagraph {
                paragraph_id: id,
                sentences: Vec::new(),
                active: false,
                opened_at: sentence.start_s,
            });
        }
        let w = self.open.as_mut().expect("paragraph just ensured");
        w.active = false;
        w.sentences.push(sentence);
        (settled, w.paragraph_id, w.sentences.clone())
    }

    /// Settle-timeout probe (call every loop tick with the current wall-clock). Closes the
    /// paragraph when it has been silent (no active speech) for ≥ `merge_gap_s` — this is how the
    /// TRAILING paragraph finalizes. Suppressed while a sentence is in progress AND while `speaking`
    /// is true — the streaming session still has a non-empty partial, i.e. someone is talking
    /// right now but this VAD's SOS for that speech hasn't arrived yet (it's RETROACTIVE, comes
    /// with EOS). Without this suppression the wall-clock timeout would fire mid-sentence and
    /// split the next sentence into a fresh paragraph — the "paragraph never has >1 sentence" bug.
    ///
    /// 空段落 GC(起音开段的配套):开段后从未出句(微弱音频,partial 一直空)→ 静默满
    /// `merge_gap_s` 即**静默丢弃**(不发事件——emit 侧对空段落本就 no-op)。真语音不会
    /// 误伤:partial 自起音 ~0.5s 起非空 → `speaking` 抑制;句一旦落地(sentences 非空)
    /// 走正常 settle 路径。不 GC 的后果:陈旧空段被很久之后的语音复用,id(时间戳)
    /// 落后于中间段落 → 客户端 id 排序错位。
    pub(crate) fn check_settle(&mut self, now: f64, speaking: bool) -> Option<SettledParagraph> {
        let w = self.open.as_ref()?;
        if w.active || speaking {
            return None;
        }
        if w.sentences.is_empty() {
            if now - w.opened_at >= self.merge_gap_s {
                self.open = None; // 空段 GC:静默丢弃,无事件
            }
            return None;
        }
        let last = w.sentences.last()?;
        if now - last.end_s >= self.merge_gap_s {
            self.take_open()
        } else {
            None
        }
    }

    /// 主动归档(用户侧"我说完了"信号):跳过 `merge_gap` 剩余等待,立即关闭开放段落。
    /// 语义与 [`Self::check_settle`] 的 suppress 条件一致 —— 有句进行中(`active`)或
    /// 段落为空时不动(调用方负责保持 flush 挂起重试);`speaking` 的墙钟抑制由调用方
    /// 判断(它不在 tracker 状态里)。
    pub(crate) fn force_settle(&mut self) -> Option<SettledParagraph> {
        let w = self.open.as_ref()?;
        if w.active || w.sentences.is_empty() {
            return None;
        }
        self.take_open()
    }

    /// 是否有开放段落(含进行中段)—— flush 挂起与否的判据:段落在 → 保持挂起等 EOS;
    /// 无段落 → flush 落空,消费掉标记。
    pub(crate) fn has_open_paragraph(&self) -> bool {
        self.open.is_some()
    }

    /// Seconds until [`Self::check_settle`] would close the open paragraph (None = no pending
    /// settle: nothing open, a sentence in progress, or `speaking` — the next
    /// sentence's speech is ongoing but its SOS hasn't arrived yet). Drives the consume loop's
    /// condvar deadline — wake exactly when the trailing paragraph (or an empty onset-opened
    /// paragraph awaiting GC) is due, not on a poll cadence.
    pub(crate) fn settle_deadline(&self, now: f64, speaking: bool) -> Option<f64> {
        let w = self.open.as_ref()?;
        if w.active || speaking {
            return None;
        }
        if w.sentences.is_empty() {
            return Some((self.merge_gap_s - (now - w.opened_at)).max(0.0));
        }
        let last = w.sentences.last()?;
        Some((self.merge_gap_s - (now - last.end_s)).max(0.0))
    }

    fn take_open(&mut self) -> Option<SettledParagraph> {
        self.open.take().map(|w| SettledParagraph {
            paragraph_id: w.paragraph_id,
            sentences: w.sentences,
        })
    }

    /// The ids the sentence currently being spoken WILL get: the open paragraph's id (or the
    /// next one when nothing is open) + the next sentence id. Used to key live `StreamFragment`
    /// partials. 正常路径下段落已在起音开启(`on_speech_onset`),partial 从第一条起就是
    /// **真实段键**;`open=None` 的兜底预测仅剩退化场景(flush 在微弱音频中切段等),
    /// 实际不可达 —— partial 只在 detected 分支发射,而 rising edge 先于任何 accept 发生。
    /// Authoritative grouping arrives with the `Batch`/`ParagraphEdge` events.
    pub(crate) fn prospective(&self) -> (ParagraphId, SentenceId) {
        let w = self
            .open
            .as_ref()
            .map(|w| w.paragraph_id)
            .unwrap_or_else(|| self.last_win_id.wrapping_add(1).max(1));
        (w, self.next_sentence_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

use super::*;

mod tests {
use super::*;

fn sentence(id: SentenceId, start_s: f64, end_s: f64) -> VadSentence {
    VadSentence {
        id,
        audio_id: id,
        start_s,
        end_s,
        streaming_text: format!("s{id}"),
        batch_text: Some(format!("b{id}")),
    }
}

#[test]
fn short_gap_absorbs_into_same_paragraph() {
    let mut t = ParagraphTracker::new(2.5);
    let s1 = t.on_sos(0.0);
    let (settled, w1, sentences) = t.on_eos(sentence(s1, 0.0, 0.5));
    assert!(settled.is_none());
    assert_eq!(sentences.len(), 1);

    // gap 1.0−0.5 = 0.5 < 2.5 → same paragraph, second sentence (merge happens at EOS,
    // where the true onset is back-derived).
    let s2 = t.on_sos(0.0);
    let (settled, w, sentences) = t.on_eos(sentence(s2, 1.0, 1.5));
    assert!(settled.is_none(), "short gap must NOT settle");
    assert_eq!(w, w1, "same paragraph continues");
    assert_eq!(sentences.len(), 2, "both sentences in one paragraph");
}

#[test]
fn big_gap_settles_previous_paragraph_and_opens_new_one() {
    let mut t = ParagraphTracker::new(2.5);
    let s1 = t.on_sos(0.0);
    let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
    // gap 5.0−0.5 = 4.5 ≥ 2.5 → settle w1 at the next sentence's EOS, open w2.
    let s2 = t.on_sos(0.0);
    let (settled, w2, sentences) = t.on_eos(sentence(s2, 5.0, 5.5));
    let s = settled.expect("big gap settles the previous paragraph");
    assert_eq!(s.paragraph_id, w1);
    assert_eq!(s.sentences.len(), 1);
    assert_ne!(w2, w1, "a fresh paragraph opens (random ids must differ)");
    assert_eq!(sentences.len(), 1);
}

#[test]
fn settle_timeout_closes_trailing_paragraph() {
    let mut t = ParagraphTracker::new(2.5);
    let s1 = t.on_sos(0.0);
    let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
    assert!(
        t.check_settle(2.0, false).is_none(),
        "2.0 − 0.5 = 1.5 < 2.5, not yet"
    );
    let s = t
        .check_settle(3.0, false)
        .expect("3.0 − 0.5 = 2.5 ≥ merge_gap → settle");
    assert_eq!(s.paragraph_id, w1);
    assert!(
        t.check_settle(10.0, false).is_none(),
        "nothing open anymore"
    );
}

#[test]
fn force_settle_skips_merge_gap_wait() {
    // 主动归档:远未到 merge_gap 也能立即关段(IME"我说完了"信号)。
    let mut t = ParagraphTracker::new(2.5);
    assert!(
        t.force_settle().is_none(),
        "无段落 → None(调用方消费掉 flush 标记)"
    );
    assert!(!t.has_open_paragraph());
    let s1 = t.on_sos(0.0);
    let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
    // 0.2s 后强制归档(gap 0.2 < merge_gap 2.5 —— 常规定稿还早)。
    let s = t.force_settle().expect("有已定稿句 → 立即归档");
    assert_eq!(s.paragraph_id, w1);
    assert_eq!(s.sentences.len(), 1);
    assert!(!t.has_open_paragraph(), "段已关");
    assert!(
        t.check_settle(100.0, false).is_none(),
        "settle 路径不再重复触发"
    );
    // 归档后再次 force → 无段落 → None。
    assert!(t.force_settle().is_none());
}

#[test]
fn force_settle_holds_while_sentence_active() {
    // 句进行中(SOS 已见 EOS 未到)→ 不动,调用方保持 flush 挂起。
    let mut t = ParagraphTracker::new(2.5);
    let s1 = t.on_sos(0.0);
    let (_, _, _) = t.on_eos(sentence(s1, 0.0, 0.5));
    let s2 = t.on_sos(0.0); // 第二句开口
    assert!(t.force_settle().is_none(), "active 句压制强制归档");
    assert!(t.has_open_paragraph(), "段落仍在 → flush 保持挂起");
    let (_, w, sentences) = t.on_eos(sentence(s2, 1.0, 1.5));
    let s = t.force_settle().expect("EOS 落定后重试成功");
    assert_eq!(s.paragraph_id, w);
    assert_eq!(sentences.len(), 2);
}

#[test]
fn settle_deadline_counts_down_to_merge_gap() {
    // The condvar wake deadline: exactly when check_settle would fire (consumes loop
    // parks on the ring condvar instead of polling — this is its only wake source for
    // the trailing paragraph).
    let mut t = ParagraphTracker::new(2.5);
    assert!(t.settle_deadline(0.0, false).is_none(), "nothing open yet");
    let s1 = t.on_sos(0.0);
    t.on_eos(sentence(s1, 0.0, 0.5));
    assert!(
        (t.settle_deadline(1.0, false).unwrap() - 2.0).abs() < 1e-9,
        "2.5 − (1.0 − 0.5)"
    );
    assert!(
        (t.settle_deadline(3.0, false).unwrap() - 0.0).abs() < 1e-9,
        "due now, clamped at 0"
    );
    let _s2 = t.on_sos(0.0); // sentence in progress (active=true)
    assert!(
        t.settle_deadline(1.2, false).is_none(),
        "active sentence ⇒ suppressed, no deadline"
    );
}

#[test]
fn active_sentence_suppresses_settle_timeout() {
    // Regression guard: a long following sentence must not be mistaken for "no
    // continuation" and force-split the paragraph mid-speech.
    let mut t = ParagraphTracker::new(2.5);
    let s1 = t.on_sos(0.0);
    t.on_eos(sentence(s1, 0.0, 0.5));
    let _s2 = t.on_sos(0.0); // sentence in progress (active=true)
    assert!(
        t.check_settle(100.0, false).is_none(),
        "active sentence ⇒ settle suppressed"
    );
}

#[test]
fn speaking_suppresses_settle_waiting_for_retroactive_sos() {
    // 回溯式 VAD 的回归防护:下一句的 SOS 要等它的 EOS 才到——在它到达前,流式
    // session 的 partial 非空(=speaking=true)必须抑制 settle 超时。否则墙钟超时
    // 会在下一句说话时定稿,把它错划进新段落(症状:段落永远只有 1 个 sentence)。
    let mut t = ParagraphTracker::new(2.5);
    let s1 = t.on_sos(0.0);
    t.on_eos(sentence(s1, 0.0, 0.5));
    // 下一句正在说话(SOS 尚未到),墙钟已远超 merge_gap —— speaking=true 抑制。
    assert!(
        t.check_settle(100.0, true).is_none(),
        "speaking ⇒ settle suppressed"
    );
    assert!(
        t.settle_deadline(100.0, true).is_none(),
        "speaking ⇒ no settle deadline"
    );
    // 说话停止(speaking=false)后,同一时刻立刻能定稿。
    assert!(
        t.check_settle(100.0, false).is_some(),
        "not speaking ⇒ settle fires"
    );
}

#[test]
fn merge_gap_zero_makes_every_sentence_its_own_paragraph() {
    let mut t = ParagraphTracker::new(0.0);
    let s1 = t.on_sos(0.0);
    let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
    // Any gap ≥ 0 settles at the next sentence's EOS (gap 0.6 − 0.5 = 0.1 ≥ 0).
    let s2 = t.on_sos(0.6);
    let (settled, w2, _) = t.on_eos(sentence(s2, 0.6, 0.7));
    assert_eq!(settled.expect("gap 0.1 ≥ 0 settles").paragraph_id, w1);
    assert_ne!(w2, w1);
    // …and the settle timeout fires immediately after an EOS too.
    let s3 = t.on_sos(10.0);
    t.on_eos(sentence(s3, 10.0, 10.5));
    assert!(
        t.check_settle(10.5, false).is_some(),
        "now − end = 0 ≥ 0 → settle"
    );
}

// ── round13:起音即开段 + 时间戳 id(§7-A/B 修复)──────────────────────

/// 起音开段 → prospective 返回**真实**段 id;该段后续所有事件(EOS 的
/// Batch/ParagraphEdge)携带同一 id —— 幽灵段(预测键 ≠ 实际键)不复存在。
#[test]
fn onset_opens_paragraph_prospective_returns_real_id() {
    let mut t = ParagraphTracker::new(2.5);
    t.on_speech_onset(10.0);
    let (pid, _sid) = t.prospective();
    let s1 = t.on_sos(10.4);
    assert_eq!(t.prospective().0, pid, "段内 prospective 稳定");
    let (settled, w, _) = t.on_eos(sentence(s1, 10.0, 10.5));
    assert!(settled.is_none());
    assert_eq!(w, pid, "EOS 归属段 = 起音开的段(prospective 即真键)");
    // 静默满 merge_gap 关段,下一次起音 → 新段(时间戳更大)。
    let _ = t
        .check_settle(20.0, false)
        .expect("静默 9.5s ≥ 2.5s → settle");
    t.on_speech_onset(20.5);
    let (pid2, _) = t.prospective();
    assert!(pid2 > pid, "时间戳 id 严格递增 —— id 即顺序");
}

/// 时间戳 id 严格递增:同微秒连续开段(防御 max(last+1))也绝不重复/回退。
#[test]
fn timestamp_win_ids_strictly_increasing() {
    let mut t = ParagraphTracker::new(2.5);
    let mut prev = 0u64;
    for i in 0..8 {
        t.on_speech_onset(i as f64);
        let (pid, _) = t.prospective();
        assert!(pid > prev, "id 必须严格递增(时间戳,防时钟回拨/同微秒)");
        prev = pid;
        // 立刻出句并关段,下一轮开新段。
        let s = t.on_sos(i as f64);
        t.on_eos(sentence(s, i as f64, i as f64 + 0.5));
        let _ = t.check_settle(i as f64 + 10.0, false);
    }
}

/// 空段 GC:起音开的段从未出句(微弱音频)→ 静默满 merge_gap 静默丢弃;
/// 不 GC 会让陈旧空段被很久之后的语音复用,id 落后 → 客户端排序错位。
#[test]
fn empty_onset_paragraph_gced_after_merge_gap() {
    let mut t = ParagraphTracker::new(2.5);
    t.on_speech_onset(0.0);
    let (pid, _) = t.prospective();
    assert!(t.check_settle(2.0, false).is_none(), "2.0 < 2.5,未到期");
    assert!(t.has_open_paragraph(), "GC 前段还在");
    assert!(
        t.check_settle(2.6, false).is_none(),
        "GC 静默:返回 None(无事件)"
    );
    assert!(!t.has_open_paragraph(), "空段静默满 merge_gap 即弃");
    // 下一次起音开**新**段(id 更大),不复用陈旧空段。
    t.on_speech_onset(100.0);
    let (pid2, _) = t.prospective();
    assert!(pid2 > pid, "新段时间戳更大");
    // settle_deadline 也覆盖空段(消费循环要能在 GC 时点醒来)。
    assert!(t.check_settle(103.0, false).is_none(), "GC 掉 100.0 的空段");
    assert!(!t.has_open_paragraph());
    t.on_speech_onset(200.0);
    let d = t.settle_deadline(201.0, false).expect("空段也有 GC 截止");
    assert!((d - 1.5).abs() < 1e-9, "2.5 − (201.0 − 200.0)");
}

/// 真语音不误伤:partial 非空(speaking)抑制空段 GC —— 长句(> merge_gap)
/// 说话中不会被墙钟 GC 掉段落。
#[test]
fn speaking_suppresses_empty_onset_gc() {
    let mut t = ParagraphTracker::new(2.5);
    t.on_speech_onset(0.0);
    assert!(t.check_settle(100.0, true).is_none());
    assert!(t.has_open_paragraph(), "speaking ⇒ 空段不 GC");
    assert!(
        t.settle_deadline(100.0, true).is_none(),
        "speaking ⇒ 无 GC 截止"
    );
    assert!(t.check_settle(100.0, false).is_none(), "静默后 GC");
    assert!(!t.has_open_paragraph());
}
}
}
