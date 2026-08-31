//! tracker — 纯边界数学(分句归属 + 段落定稿决策,零 I/O,可单测):
//! 哪句属于哪段(id = 时间戳,严格递增)、merge_gap 何时关段、起音即开段、空段 GC。
//! 识别侧(流式会话、batch、AudioStore)都在 recognizer;这里只有 SOS/EOS 之上的
//! 段落划分决策。

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

/// A paragraph closed by a big gap or the settle-timeout — the recognizer turns this into a
/// [`VadParagraph`] (concat PCM + paragraph-level batch re-run) and emits `ParagraphEdge`.
pub(crate) struct SettledParagraph {
    pub(crate) paragraph_id: ParagraphId,
    pub(crate) sentences: Vec<VadSentence>,
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
    /// decision moves to [`Self::on_eos`], which back-derives the true speech onset from the PCM.
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
    /// sentence ≥ merge_gap (using `sentence.start_s`, the BACK-DERIVED true onset), then pushes this
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

