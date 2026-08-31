//! transcript.rs — 语音识别事件的**折叠状态机**(round11 从 ime-core 的
//! voice_state.rs 归位至此:五类 `AsrEvent` 的段落组装是 aura 协议细节,
//! 属于 aura 的客户端 SDK;上层(IME)只读高级状态,不关注事件细节)。
//!
//! ## 核心语义:段落 id 即顺序(跨段乱序鲁棒)
//!
//! aura-daemon 的识别事件来自**两个发射源**(`aura-pipeline` 线程直发
//! `StreamFragment`,`aura-stage2` 线程直发 batch/校准事件,见
//! `aura-core/pipeline.rs::run`),**到达顺序没有跨段保证**:新段落的流式
//! 事件可能先于上一段落的迟到定稿到达(LLM 校准秒级延迟)。
//!
//! 本状态机因此不信任到达顺序,改用 `paragraph_id`(daemon 端单调递增)
//! 作为唯一顺序信号 —— `BTreeMap<paragraph_id, ParaState>` 持段:
//!
//! - **首选组合预览只跟"当前正在说的段"**(id 最大的未关闭段落):说过
//!   的段落不出现在首选里 —— 用户继续说下一段时,首选干净切换,**绝不
//!   堆叠拼接**(S2 初版"未定稿段全拼首选"在实测中造成重复混乱,已废);
//! - **段落关闭即进 finals**(batch/拼接文本占位;定稿到达后**替换**为
//!   校准文本 —— 协议的 REPLACED 语义):说过的话永远可见可提交,不因
//!   跨段失序而消失;
//! - **finals 按段 id 排序**:迟到定稿归位到正确顺序(按到达序头插在乱序
//!   时会颠倒 finals)。
//!
//! ## 优雅降级(batch 不可靠、流式可靠)
//!
//! 流式(本地)是**底线**:live 只由 `StreamFragment` 写入;句/段落的 batch
//! 文本在任何形式的缺席下(remote 端点掉线、识别失败、迟到)都逐级回退
//! 流式拼接。校准(LLM)只增强 calc 预览与定稿,永不污染 live/plain。
//!
//! ## Thread safety
//!
//! `SharedTranscript` = `Mutex<Transcript>` + conn/mock 原子量。纯
//! `std::sync`,不依赖 tokio runtime,任何线程可读可写。写入端:IoThread
//! 上的 voice 会话(SSE 事件到达时 `fold_event`);读取端:IME 主线程
//! (key event / tick),锁只保护微秒级字符串 clone。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

/// Max retained settled utterances (candidate slots)。
pub const MAX_FINALS: usize = 8;

/// 一个句子的识别状态(供 `ParaState` 折叠)。**自带句 id**(`segment_id`)——
/// 事件匹配(`s.id == event.sentence_id`)与调试自述都靠它,不再挂在容器外壁。
#[derive(Debug, Clone)]
struct SentenceState {
    id: u64,
    /// 最近一次 `StreamFragment` 文本。
    stream: String,
    /// `BatchSentence` 结果(EOS 出 batch 后才有)。
    batch: Option<String>,
}

/// 一个段落的识别状态。**自带段 id**(`window_id`)—— 与 BTreeMap 的 key 互为
/// 镜像(结构体自述身份,`finals`/调试直接用 `p.id`)。
#[derive(Debug, Clone)]
struct ParaState {
    id: u64,
    /// 整段已关闭(`BatchParagraph` / `ParagraphCalibration` 到达)。
    closed: bool,
    /// 整段 batch 重跑文本(`BatchParagraph`,权威 raw)。
    batch_paragraph: Option<String>,
    /// 定稿校准文本(`ParagraphCalibration`;非空 = 该段已进 finals)。
    calibrated: Option<String>,
    /// Stage2 对当前段落所有已到 Sentence 的临时联合校准(`SentenceCalibration`)。
    sentence_calibration: Option<String>,
    /// SC 的覆盖上界 —— **事件自带的句 id**(`segment_calibration.segment_id`,
    /// round20b:触发该次纠偏的 BS 句;server 直接带下,客户端零派生状态)。
    /// 过界的新句(正在说的)以最优文本续接在 SC 之后,不被陈旧 SC 遮住。
    sc_covers_sid: Option<u64>,
    /// 各 Sentence,按首见顺序(自带 id,`s.id` 匹配事件)。
    sentences: Vec<SentenceState>,
}

impl ParaState {
    fn new(id: u64) -> Self {
        ParaState {
            id,
            closed: false,
            batch_paragraph: None,
            calibrated: None,
            sentence_calibration: None,
            sc_covers_sid: None,
            sentences: Vec::new(),
        }
    }

    /// 逐句拼接:每句有 `BatchSentence` 用 `BatchSentence`,否则 `StreamFragment`。
    fn concat_sentences(&self) -> String {
        let mut out = String::new();
        for ev in &self.sentences {
            let text = ev.batch.clone().unwrap_or_else(|| ev.stream.clone());
            out.push_str(&text);
        }
        out
    }

    /// plain 的段内 best:`BatchParagraph`(整窗权威 batch)优先于逐句拼接。
    fn best_plain(&self) -> String {
        self.batch_paragraph
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| self.concat_sentences())
    }

    /// calc 的段内 best:窗口已关闭 → `ParagraphCalibration` 优先于
    /// `BatchParagraph`;未关闭 → `SentenceCalibration`(**只顶替它覆盖的部分**,
    /// 覆盖上界 `sc_covers_sid`;之后的新句 —— 正在说的 —— 以 batch>流式 续接,
    /// 陈旧 SC 不得遮住新句,round20)优先于逐句拼接。
    fn best_calc(&self) -> String {
        if self.closed {
            return self
                .calibrated
                .clone()
                .or_else(|| self.batch_paragraph.clone())
                .unwrap_or_default();
        }
        match &self.sentence_calibration {
            Some(sc) if !sc.is_empty() => {
                let cover = self.sc_covers_sid.unwrap_or(0);
                let mut out = sc.clone();
                for st in &self.sentences {
                    if st.id > cover {
                        out.push_str(&st.batch.clone().unwrap_or_else(|| st.stream.clone()));
                    }
                }
                out
            }
            _ => self.concat_sentences(),
        }
    }

    /// 该段是否已定稿(calibrated 非空 —— 定稿文本已可用)。
    fn settled(&self) -> bool {
        self.calibrated.as_ref().is_some_and(|t| !t.is_empty())
    }

    /// 定稿文本(未定稿 → None)。
    fn settled_text(&self) -> Option<String> {
        self.calibrated
            .clone()
            .filter(|t| !t.is_empty())
    }
}

/// 纯折叠状态机(零 tokio 零锁;经 [`SharedTranscript`] 共享)。
#[derive(Default)]
pub struct Transcript {
    /// 段落状态,**id 即顺序**(时间戳,单调递增;跨段乱序鲁棒的根基)。
    paragraphs: BTreeMap<u64, ParaState>,
    /// 当前流式文本(`StreamFragment` raw)。
    live: String,
    /// `live` 的来源段 —— live 只能被**它自己段**的关闭/定稿清掉:跨段交错
    /// (段 N 的 PCal 迟到时用户已在说段 N+1)不得误清新段正在说的 partial;
    /// 同理,段关闭时若 live 属于该段,立即清空(首选让位 finals,不残留旧句)。
    live_pid: Option<u64>,
}

impl Transcript {
    /// 取/建段落状态(id 落进结构体,与 map key 互为镜像 —— debug 断言钉死)。
    fn para(&mut self, id: u64) -> &mut ParaState {
        let p = self.paragraphs.entry(id).or_insert_with(|| ParaState::new(id));
        debug_assert_eq!(p.id, id, "ParaState.id 与 map key 必须互为镜像");
        p
    }

    /// 折叠一个 `AsrEvent`(SSE 数据面到达时由写入端调用)。
    pub fn fold(&mut self, ev: &crate::view::AsrEvent) {
        match ev {
            crate::view::AsrEvent::StreamFragment {
                paragraph_id,
                sentence_id,
                text,
                ..
            } => {
                // 字段直访(不相交借用):本臂随后还要写 self.live/self.live_pid,
                // 不能走借用整个 self 的 para()。
                let entry = self
                    .paragraphs
                    .entry(*paragraph_id)
                    .or_insert_with(|| ParaState::new(*paragraph_id));
                // **已关闭段的迟到 SF 完全忽略**(round15 回归:首选"batch 后退回
                // 流式")。句槽不写只是防御;关键是 **live 不能写** —— 首选兜底链是
                // `plain_preview().or(live)`,段关闭后 plain 为空,live 被这条陈旧
                // 流式 partial 污染即回退。迟到句的完整文本由它实际归属段的事件携带。
                if entry.closed {
                    return;
                }
                self.live = text.clone();
                self.live_pid = Some(*paragraph_id);
                upsert_sentence(&mut entry.sentences, *sentence_id, |s| {
                    s.stream = text.clone();
                });
            }
            crate::view::AsrEvent::BatchSentence {
                paragraph_id,
                sentence_id,
                text,
            } => {
                let entry = self.para(*paragraph_id);
                upsert_sentence(&mut entry.sentences, *sentence_id, |s| {
                    s.batch = Some(text.clone());
                });
            }
            crate::view::AsrEvent::ParagraphClosed { paragraph_id } => {
                // **server 保证的边界时序**(round11 S3):本事件先于下一段的
                // 任何事件到达。标记该段关闭 —— finals 立即以流式/拼接文本
                // 占位;后续 `BatchParagraph` / `ParagraphCalibration` 按 id
                // 修订替换(REPLACED)。
                let entry = self.para(*paragraph_id);
                entry.closed = true;
                // live 属于本段(正常:PC 先于下一段任何事件)→ 立即清空,
                // 首选让位 finals,不残留该段最后一句在首选上。
                if self.live_pid == Some(*paragraph_id) {
                    self.live.clear();
                    self.live_pid = None;
                }
            }
            crate::view::AsrEvent::BatchParagraph { paragraph_id, text } => {
                let entry = self.para(*paragraph_id);
                entry.closed = true;
                entry.batch_paragraph = Some(text.clone());
            }
            crate::view::AsrEvent::SentenceCalibration {
                paragraph_id,
                sentence_id,
                calibrated,
            } => {
                // 校准不写 `live` —— live 是流式底线(本地、可靠),迟到的校准
                // 文本把 live 倒退回旧句会造成显示抖动。校准只进窗口状态。
                // 覆盖上界直接取事件自带的句 id(round20b):SC 只覆盖 ≤ 它的
                // 已关闭句,正在说的新句不在内 —— best_calc 把过界新句续接而非遮住。
                let entry = self.para(*paragraph_id);
                entry.sentence_calibration = Some(calibrated.clone());
                entry.sc_covers_sid = Some(*sentence_id);
            }
            crate::view::AsrEvent::ParagraphCalibration {
                paragraph_id,
                calibrated,
            } => {
                // live 只在属于本段时清:跨段交错(本段定稿迟到、新段已在说)
                // 不得误清新段的 partial。
                if self.live_pid == Some(*paragraph_id) {
                    self.live.clear();
                    self.live_pid = None;
                }
                let entry = self.para(*paragraph_id);
                entry.closed = true;
                entry.calibrated = Some(calibrated.clone());
                // finals 不再单独缓存 —— `finals()` 按段 id 动态收集,迟到
                // 定稿自动归位到正确顺序(到达序头插在乱序时会颠倒 finals)。
            }
            crate::view::AsrEvent::Correction { .. } => {
                // Stage2 correction feedback 暂不处理(后续通过 AuraClient::correct 单独触发)。
            }
        }
    }

    /// finals 候选历史,**最新在前**(按段 id 降序 —— id 即说话顺序),
    /// 截断 [`MAX_FINALS`]。
    ///
    /// 收录范围:**已关闭**(BatchParagraph / ParagraphCalibration 到达)或
    /// 已校准的段落 —— 段落关闭即进 finals(batch/拼接文本占位),定稿到达
    /// 后替换为校准文本(REPLACED 语义)。说过的话永远可见可提交,不因
    /// 跨段失序消失。
    pub fn finals(&self) -> Vec<String> {
        self.paragraphs
            .iter()
            .rev()
            .filter(|(_, p)| p.closed || p.settled())
            .map(|(_, p)| p.settled_text().unwrap_or_else(|| p.best_plain()))
            .filter(|t| !t.is_empty())
            .take(MAX_FINALS)
            .collect()
    }

    /// 当前流式文本(组装预览的原始兜底)。
    pub fn live(&self) -> String {
        self.live.clone()
    }

    /// **首选组合预览(plain)**:只跟"当前正在说的段" —— id 最大的
    /// **未关闭**段落的 `best_plain`。
    ///
    /// 段落关闭(BatchParagraph/定稿到达)即从首选退出(它已在
    /// [`Self::finals`] 里)—— 用户继续说下一段时首选干净切换,**绝不把
    /// 多段堆叠拼接**(S2 初版实测把未定稿段全拼进首选,造成前后句重复)。
    /// 无未关闭段落(都关了、定稿在路上)→ 首选为空,候选由 finals 承接。
    pub fn plain_preview(&self) -> String {
        self.active_paragraph().map(|p| p.best_plain()).unwrap_or_default()
    }

    /// **校准优先预览(calc)**:当前活动段落的 `best_calc`(未关闭 →
    /// `SentenceCalibration` 优先于流式拼接);无活动段落 → 最新定稿校准。
    pub fn calc_preview(&self) -> String {
        if let Some(p) = self.active_paragraph() {
            return p.best_calc();
        }
        self.paragraphs
            .iter()
            .rev()
            .find(|(_, p)| p.settled())
            .and_then(|(_, p)| p.calibrated.clone())
            .unwrap_or_default()
    }

    /// **live 级联预览**(架构规则:纠偏 > batch > 流式)。活动段 = 该段的
    /// `best_calc`(SC 非空 → SC;否则逐句 batch 优先、流式兜底的拼接 —— 只有最新
    /// 那句还停留在流式,老句早已被 batch/纠偏顶替);无活动段(段刚关、下一句
    /// 未起)回退原始 live 流式。
    pub fn cascade_preview(&self) -> String {
        match self.active_paragraph() {
            Some(p) => p.best_calc(),
            None => self.live.clone(),
        }
    }

    /// 旧的展开兜底:最新 final > 组合预览 > live > ""。
    pub fn snapshot(&self) -> String {
        self.finals()
            .first()
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let p = self.plain_preview();
                if !p.is_empty() {
                    return p;
                }
                self.live.clone()
            })
    }

    /// 全部清空(重连 / 显式 reset)。
    pub fn clear(&mut self) {
        self.paragraphs.clear();
        self.live.clear();
        self.live_pid = None;
    }

    /// 重连后**全量同步 aura 历史定稿**(`GET /api/results`,最旧 → 最新):
    /// 每条非空 calibrated 作为已定稿段落灌入(id 定位,顺序天然正确)。
    /// 数据面 SSE 是 append-only,重连收不到断连期间的历史 —— 靠这里补齐。
    pub fn sync_history(&mut self, history: &[(u64, String)]) {
        self.clear();
        for (id, calibrated) in history {
            if calibrated.is_empty() {
                continue;
            }
            let entry = self.para(*id);
            entry.closed = true;
            entry.calibrated = Some(calibrated.clone());
        }
    }

    /// 测试 / mock 用的流式文本注入(不更新段落状态)。生产代码永远走 `fold`。
    pub fn set_live_raw(&mut self, text: &str) {
        self.live = text.to_string();
    }

    /// 测试 / mock 用的种子 final:注入一个合成的已定稿段落(不等同于真实
    /// `ParagraphCalibration` 的完整流程)。
    pub fn seed_final(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let id = self.paragraphs.last_key_value().map_or(0, |(k, _)| k + 1);
        let entry = self.para(id);
        entry.closed = true;
        entry.calibrated = Some(text.to_string());
        self.live.clear();
    }

    /// 当前活动段落(id 最大的未关闭段落 —— 用户正在说的)。
    fn active_paragraph(&self) -> Option<&ParaState> {
        self.paragraphs
            .iter()
            .rev()
            .find(|(_, p)| !p.closed)
            .map(|(_, p)| p)
    }
}

/// Aura 连接状态(三态):正在连接 / 已连接 / 失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VoiceConn {
    /// 正在连接 / 未连上:候选显示「正在连接语音服务」。
    Connecting = 0,
    /// 已连接:候选显示 🎙 麦克风图标。
    Connected = 1,
    /// 连接失败 / 流已断:候选显示「语音服务暂不可用」。
    Failed = 2,
}

/// 共享的语音会话状态:`Transcript` + 连接三态 + mock 标志。
/// `Arc<SharedTranscript>` 在写入端(IoThread 上的 voice 会话)与读取端
/// (IME 主线程)之间共享。
pub struct SharedTranscript {
    inner: Mutex<Transcript>,
    /// Aura daemon 连通性三态(health 探针 / SSE 流事件写入)。
    conn: AtomicU8,
    /// Mock 模式(宿主调试,如 IME 的 `--asr-text`):冻结 conn / finals ——
    /// 写入端不连接 aura、`set_conn` 不覆盖,seed 的数据稳定可见(与真实
    /// 事件流打架会让 mock 候选闪没/被 sync_history 换成真实数据)。
    mock: AtomicBool,
}

impl SharedTranscript {
    pub fn new() -> Self {
        SharedTranscript {
            inner: Mutex::new(Transcript::default()),
            conn: AtomicU8::new(VoiceConn::Connecting as u8),
            mock: AtomicBool::new(false),
        }
    }

    /// 进入/退出 mock 模式:true 时 `set_conn` 冻结、写入端不发起真实连接。
    pub fn set_mock(&self, on: bool) {
        self.mock.store(on, Ordering::Relaxed);
    }

    /// 是否处于 mock 模式。
    pub fn is_mock(&self) -> bool {
        self.mock.load(Ordering::Relaxed)
    }

    // ── Connectivity(三态)──────────────────────────────────────────────

    pub fn set_conn(&self, c: VoiceConn) {
        if self.is_mock() {
            return; // mock:seed 的 Connected 不被探针/流回调覆盖
        }
        self.conn.store(c as u8, Ordering::Relaxed);
    }

    pub fn conn(&self) -> VoiceConn {
        // 按枚举判别值还原(与 `set_conn` 的 `c as u8` 对应,无常量魔法数)。
        match self.conn.load(Ordering::Relaxed) {
            x if x == VoiceConn::Connected as u8 => VoiceConn::Connected,
            x if x == VoiceConn::Failed as u8 => VoiceConn::Failed,
            _ => VoiceConn::Connecting,
        }
    }

    /// 兼容布尔判断:仅 `Connected` 视为"已连接"。
    pub fn is_connected(&self) -> bool {
        self.conn() == VoiceConn::Connected
    }

    // ── State accessors(锁内微秒级 clone)─────────────────────────────

    /// 折叠一个 SSE 事件(写入端在事件到达时调用)。
    pub fn fold_event(&self, ev: &crate::view::AsrEvent) {
        self.inner.lock().unwrap().fold(ev);
    }

    /// 已定稿文本,最新在前(按段 id 序,截断 [`MAX_FINALS`])。
    pub fn finals(&self) -> Vec<String> {
        self.inner.lock().unwrap().finals()
    }

    /// 当前流式文本。
    pub fn live(&self) -> String {
        self.inner.lock().unwrap().live()
    }

    /// 首选组合预览(plain;详见 [`Transcript::plain_preview`])。
    pub fn plain_preview(&self) -> String {
        self.inner.lock().unwrap().plain_preview()
    }

    /// 校准优先预览(calc;详见 [`Transcript::calc_preview`])。
    pub fn calc_preview(&self) -> String {
        self.inner.lock().unwrap().calc_preview()
    }

    /// **live 级联预览**(纠偏 > batch > 流式;详见 [`Transcript::cascade_preview`])。
    pub fn cascade_preview(&self) -> String {
        self.inner.lock().unwrap().cascade_preview()
    }

    /// 旧的展开兜底:最新 final > 组合预览 > live > ""。
    pub fn snapshot(&self) -> String {
        self.inner.lock().unwrap().snapshot()
    }

    /// **清空全部状态**(重连后调用,避免断连期间的旧句残留)。
    pub fn reset(&self) {
        self.inner.lock().unwrap().clear();
    }

    /// 重连后全量同步 aura 历史定稿(最旧 → 最新;按段 id 归位)。
    pub fn sync_history(&self, history: &[(u64, String)]) {
        self.inner.lock().unwrap().sync_history(history);
    }

    /// 测试 / mock 注入。
    pub fn set_live_raw(&self, text: &str) {
        self.inner.lock().unwrap().set_live_raw(text);
    }

    /// 测试 / mock 种子 final。
    pub fn seed_final(&self, text: &str) {
        self.inner.lock().unwrap().seed_final(text);
    }
}

impl Default for SharedTranscript {
    fn default() -> Self {
        Self::new()
    }
}

fn upsert_sentence(
    sentences: &mut Vec<SentenceState>,
    sentence_id: u64,
    f: impl FnOnce(&mut SentenceState),
) {
    if let Some(s) = sentences.iter_mut().find(|s| s.id == sentence_id) {
        f(s);
    } else {
        let mut s = SentenceState { id: sentence_id, stream: String::new(), batch: None };
        f(&mut s);
        sentences.push(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::AsrEvent;

    fn stream(wid: u64, sid: u64, text: &str) -> AsrEvent {
        AsrEvent::StreamFragment {
            paragraph_id: wid,
            sentence_id: sid,
            text: text.into(),
            at_s: 0.0,
        }
    }

    fn batch_sentence(wid: u64, sid: u64, text: &str) -> AsrEvent {
        AsrEvent::BatchSentence {
            paragraph_id: wid,
            sentence_id: sid,
            text: text.into(),
        }
    }

    fn batch_paragraph(wid: u64, text: &str) -> AsrEvent {
        AsrEvent::BatchParagraph {
            paragraph_id: wid,
            text: text.into(),
        }
    }

    fn sentence_cal(wid: u64, sid: u64, text: &str) -> AsrEvent {
        AsrEvent::SentenceCalibration {
            paragraph_id: wid,
            sentence_id: sid,
            calibrated: text.into(),
        }
    }

    fn win_cal(wid: u64, text: &str) -> AsrEvent {
        AsrEvent::ParagraphCalibration {
            paragraph_id: wid,
            calibrated: text.into(),
        }
    }

    // ── 基本折叠(自 voice_state.rs 迁移,API 对齐)────────────────────

    #[test]
    fn stream_then_window_final_pushes_final_clears_live() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "ni"));
        s.fold_event(&batch_sentence(1, 1, "你好"));
        s.fold_event(&win_cal(1, "你好世界"));
        assert_eq!(s.finals(), vec!["你好世界"]);
        assert_eq!(s.live(), "");
        // 段已关闭 → 首选空(定稿在 finals 里),候选由 finals 承接。
        assert_eq!(s.plain_preview(), "");
        assert_eq!(s.calc_preview(), "你好世界");
    }

    #[test]
    fn open_paragraph_preview_concats_sentences_in_order() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "第一段"));
        s.fold_event(&batch_sentence(1, 1, "第一段"));
        s.fold_event(&stream(1, 2, "第二段"));
        assert_eq!(s.plain_preview(), "第一段第二段");
    }

    #[test]
    fn closed_paragraph_enters_finals_with_batch_text() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "raw"));
        s.fold_event(&batch_sentence(1, 1, "raw"));
        s.fold_event(&batch_paragraph(1, "整窗batch"));
        // 段关闭即进 finals(batch 文本占位)——说过的话可见可提交。
        assert_eq!(s.finals(), vec!["整窗batch"]);
        // 首选让位:关闭的段不再出现在首选。
        assert_eq!(s.plain_preview(), "");
        // 定稿到达 → 替换 finals 条目(REPLACED 语义)。
        s.fold_event(&win_cal(1, "整窗校准"));
        assert_eq!(s.finals(), vec!["整窗校准"]);
    }

    #[test]
    fn calc_prefers_window_calibration_over_batch_window() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "raw"));
        s.fold_event(&batch_sentence(1, 1, "raw"));
        s.fold_event(&batch_paragraph(1, "整窗batch"));
        // 无活动段、未定稿 → calc 空;/calc 兜底 finals(voice.rs 承接)。
        assert_eq!(s.calc_preview(), "");
        s.fold_event(&win_cal(1, "定稿"));
        assert_eq!(s.calc_preview(), "定稿");
    }

    #[test]
    fn calc_open_prefers_sentence_calibration_over_concat() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "第一段"));
        s.fold_event(&batch_sentence(1, 1, "第一段"));
        s.fold_event(&stream(1, 2, "第二段"));
        // 阶段2 联合校准到达 → calc 走它。round20:SC 只覆盖 ≤ 触发它的 BS 句
        //(s1),正在说的 s2 不在覆盖内 → **续接**在 SC 之后,不被遮住。
        s.fold_event(&sentence_cal(1, 1, "联合整流"));
        assert_eq!(s.calc_preview(), "联合整流第二段");
        // 未关闭,plain 走 concat(第一段 batch, 第二段 stream)。
        assert_eq!(s.plain_preview(), "第一段第二段");
    }

    #[test]
    fn finals_capped_newest_first() {
        let s = SharedTranscript::new();
        for i in 0..(MAX_FINALS + 3) as u64 {
            s.fold_event(&win_cal(i + 1, &format!("句{}", i + 1)));
        }
        assert_eq!(s.finals().len(), MAX_FINALS);
        assert_eq!(s.finals()[0], format!("句{}", MAX_FINALS + 3));
        assert_eq!(s.finals().last().unwrap(), "句4");
    }

    #[test]
    fn snapshot_prefers_latest_final() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "流式中"));
        s.fold_event(&win_cal(1, "定稿一"));
        s.fold_event(&stream(2, 2, "流式二"));
        assert_eq!(s.snapshot(), "定稿一");
    }

    #[test]
    fn connectivity_isolated_from_data() {
        let s = SharedTranscript::new();
        assert_eq!(s.conn(), VoiceConn::Connecting);
        s.set_conn(VoiceConn::Connected);
        assert!(s.is_connected());
        assert_eq!(s.conn(), VoiceConn::Connected);
        s.set_conn(VoiceConn::Failed);
        assert!(!s.is_connected());
        assert_eq!(s.conn(), VoiceConn::Failed);
    }

    #[test]
    fn empty_calibration_keeps_raw_text_in_finals() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "hi"));
        s.fold_event(&win_cal(1, ""));
        // 空校准 = 该段没有可用定稿(batch 失败等)→ finals 保留 raw/batch
        // 占位文本,不产生空条目、也不让段落消失。
        assert_eq!(s.finals(), vec!["hi"]);
    }

    // ── round11 新增:跨段乱序鲁棒(id 即顺序)─────────────────────────

    /// **回归(Bug 2 原始形态)**:段 1 关闭后段 2 开流(定稿未到)→
    /// 段 1 以 batch 文本留在 finals(不消失),首选干净切到段 2 ——
    /// 不覆盖、不堆叠。
    #[test]
    fn closed_paragraph_stays_in_finals_while_next_opens() {
        let s = SharedTranscript::new();
        // 段 1:流式 → 关闭(定稿未到)。
        s.fold_event(&stream(1, 1, "第一句"));
        s.fold_event(&batch_paragraph(1, "第一句整窗"));
        // 失序窗口:段 2 的流式先到,段 1 的 ParagraphCalibration 还在路上。
        s.fold_event(&stream(2, 1, "第二句"));
        // 首选只跟活动段(段 2)—— 不与段 1 拼接堆叠。
        assert_eq!(s.plain_preview(), "第二句");
        // 段 1 以 batch 文本留在 finals —— 永远可见可提交。
        assert_eq!(s.finals(), vec!["第一句整窗"]);
        // 迟到定稿到达 → finals 条目替换为校准文本。
        s.fold_event(&win_cal(1, "第一句校准"));
        assert_eq!(s.finals(), vec!["第一句校准"]);
        assert_eq!(s.plain_preview(), "第二句");
    }

    /// **回归(实测重复混乱,S2 初版缺陷)**:连续两段都未收到定稿时,
    /// 首选只含当前活动段,绝不拼接历史段。
    #[test]
    fn preview_never_stacks_multiple_paragraphs() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "喂，喂，喂。现在出现了更严重的问题了啊"));
        s.fold_event(&batch_paragraph(1, "喂，喂，喂。现在出现了更严重的问题了啊"));
        // 段 2 开流(段 1 定稿仍未到)。
        s.fold_event(&stream(2, 1, "喂喂现在出现了更严重的问题了啊！"));
        // 候选 = finals[0](段 1 batch)+ 首选(段 2 流式)—— 各自独立。
        assert_eq!(s.finals(), vec!["喂，喂，喂。现在出现了更严重的问题了啊"]);
        assert_eq!(s.plain_preview(), "喂喂现在出现了更严重的问题了啊！");
    }

    /// **S3 边界信号**:ParagraphClosed(时序保证的关闭事件)即进 finals
    /// 占位(流式拼接),后续 batch/校准按 id 修订替换 —— 三级渐进。
    #[test]
    fn paragraph_closed_marks_boundary_then_revisions_replace() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "第一"));
        s.fold_event(&stream(1, 2, "第二句流式"));
        // 边界信号(先于段 2 到达 —— server 时序保证)。
        s.fold_event(&AsrEvent::ParagraphClosed { paragraph_id: 1 });
        // 关闭即定稿占位(流式拼接);首选让位。
        assert_eq!(s.finals(), vec!["第一第二句流式"]);
        assert_eq!(s.plain_preview(), "");
        // 段 2 开流。
        s.fold_event(&stream(2, 1, "新段"));
        assert_eq!(s.plain_preview(), "新段");
        assert_eq!(s.finals(), vec!["第一第二句流式"]);
        // 整窗 batch 修订到达(可能乱序)→ 替换占位。
        s.fold_event(&batch_paragraph(1, "整窗batch"));
        assert_eq!(s.finals(), vec!["整窗batch"]);
        // 校准修订到达 → 最终替换。
        s.fold_event(&win_cal(1, "定稿校准"));
        assert_eq!(s.finals(), vec!["定稿校准"]);
        assert_eq!(s.plain_preview(), "新段");
    }

    /// serde:ParagraphClosed 线协议往返(tag `paragraph_closed`,wire key
    /// `window_id`)。
    #[test]
    fn paragraph_closed_serde_roundtrip() {
        let ev: AsrEvent =
            serde_json::from_str(r#"{"type":"paragraph_closed","window_id":7}"#).expect("parse");
        match ev {
            AsrEvent::ParagraphClosed { paragraph_id } => assert_eq!(paragraph_id, 7),
            other => panic!("wrong variant: {other:?}"),
        }
        let json = serde_json::to_string(&AsrEvent::ParagraphClosed { paragraph_id: 9 }).unwrap();
        assert!(json.contains(r#""type":"paragraph_closed""#), "{json}");
        assert!(json.contains(r#""window_id":9"#), "{json}");
    }

    /// **回归(次级 bug)**:定稿乱序到达(段 2 定稿先于段 1 定稿)→
    /// finals 按段 id 排序,不得按到达序颠倒。
    #[test]
    fn finals_sorted_by_paragraph_id_not_arrival_order() {
        let s = SharedTranscript::new();
        // 先说段 1、段 2,两个定稿**乱序**到达(2 先、1 后)。
        s.fold_event(&stream(1, 1, "一"));
        s.fold_event(&stream(2, 1, "二"));
        s.fold_event(&win_cal(2, "第二句"));
        s.fold_event(&win_cal(1, "第一句"));
        // finals 按段 id 降序(最新在前)= 段 2 文本在前,尽管段 1 定稿后到。
        assert_eq!(s.finals(), vec!["第二句", "第一句"]);
        assert_eq!(s.snapshot(), "第二句");
    }

    /// **live 级联(架构:纠偏 > batch > 流式)**:活动段内逐句状态机刷新,
    /// 最新句停在流式、老句被 batch 顶替、整段被纠偏(SC)顶替;无活动段回退 live。
    #[test]
    fn cascade_preview_prefers_calibration_over_batch_over_stream() {
        let s = SharedTranscript::new();
        // 句1:流式+batch;句2(最新):仅流式。
        s.fold_event(&stream(1, 1, "流式一"));
        s.fold_event(&batch_sentence(1, 1, "批式一"));
        s.fold_event(&stream(1, 2, "流式二"));
        // 无 SC → batch>流式 的逐句拼接(最新句停在流式)。
        assert_eq!(s.cascade_preview(), "批式一流式二");
        // SC 到达(覆盖 s1,触发它的 BS 是 s1)→ 顶替覆盖内部分;
        // **正在说的 s2(过界新句)续接,不被陈旧 SC 遮住**(round20 回归:
        // 实测第二句流式不刷新 UI 的根因)。
        s.fold_event(&sentence_cal(1, 1, "纠偏一二"));
        assert_eq!(s.cascade_preview(), "纠偏一二流式二");
        // 新句继续生长 → 尾巴持续刷新。
        s.fold_event(&stream(1, 2, "流式二还在长"));
        assert_eq!(s.cascade_preview(), "纠偏一二流式二还在长");
        // 段关闭(无活动段)→ 回退 live;PC 已清 live → 空。
        s.fold_event(&AsrEvent::ParagraphClosed { paragraph_id: 1 });
        assert_eq!(s.cascade_preview(), "");
        // 下一句起音,live 恢复流式。
        s.fold_event(&stream(2, 1, "新段流式"));
        assert_eq!(s.cascade_preview(), "新段流式");
    }

    /// **回归(round15 实测:首选 流式→batch→又退回流式)**。路径:迟到的 SF(段已
    /// 关闭,服务端 onset-vs-deadline 盲区 race 的产物)不得污染 live —— 首选兜底链
    /// `plain.or(live)` 在段关闭后 plain 为空,live 被陈旧流式写入即回退。
    #[test]
    fn late_stream_fragment_for_closed_paragraph_never_pollutes_live() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "流式一"));
        s.fold_event(&batch_sentence(1, 1, "批式一"));
        assert_eq!(s.plain_preview(), "批式一", "级联:batch 优先于流式");
        s.fold_event(&AsrEvent::ParagraphClosed { paragraph_id: 1 });
        assert_eq!(s.finals(), vec!["批式一"]);
        assert_eq!(s.plain_preview(), "");
        // 迟到的 SF(段已关):完全忽略 —— 不写句槽,更不写 live。
        s.fold_event(&stream(1, 2, "迟到partial"));
        assert_eq!(s.live(), "", "closed 段的 SF 不写 live");
        assert_eq!(s.plain_preview(), "", "首选不回退到陈旧流式");
        assert_eq!(s.finals(), vec!["批式一"], "finals 不受污染");
    }

    /// **级联优先级不变式(用户要求确认)**:同句 SF 在 BS 之后到达(任何乱序),
    /// 拼接级联仍用 batch —— 流式结果不会把 preview 的优先级计算顶回去。
    #[test]
    fn cascade_never_reverts_sentence_to_stream_after_batch() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "流式A"));
        s.fold_event(&batch_sentence(1, 1, "批式A"));
        // 同句迟到的流式更新:batch 槽位不被覆写。
        s.fold_event(&stream(1, 1, "流式A迟到的同句更新"));
        assert_eq!(s.plain_preview(), "批式A");
        // 多句共存:前句 batch + 后句流式,各按各的优先级。
        s.fold_event(&stream(1, 2, "流式B"));
        assert_eq!(s.plain_preview(), "批式A流式B");
        // 整段 batch(BP)仍优先于逐句拼接。
        s.fold_event(&batch_paragraph(1, "整段批"));
        assert_eq!(s.plain_preview(), "", "段关闭,首选让位");
        assert_eq!(s.finals(), vec!["整段批"]);
    }

    /// sync_history 按 id 归位:重连补历史后,后续迟到定稿不破坏顺序。
    #[test]
    fn sync_history_ids_align_with_late_finals() {
        let s = SharedTranscript::new();
        // 断连期间的历史(旧→新):1、2。
        s.sync_history(&[(1, "历史一".into()), (2, "历史二".into())]);
        assert_eq!(s.finals(), vec!["历史二", "历史一"]);
        // 重连后新流(段 3)+ 段 3 定稿。
        s.fold_event(&stream(3, 1, "新段"));
        s.fold_event(&win_cal(3, "新段定稿"));
        assert_eq!(s.finals(), vec!["新段定稿", "历史二", "历史一"]);
    }

    // ── live 归属段(live_pid):跨段交错不误清 + 关段即让位 ──────────────

    /// **回归(round13 前端审计 F1)**:段 N 的定稿迟到(PCal 物理晚于新段
    /// 起音)→ 不得误清新段正在说的 live/首选。
    #[test]
    fn late_paragraph_calibration_keeps_new_paragraph_live() {
        let s = SharedTranscript::new();
        // 段 1 定稿链尾还在路上,用户已开始说段 2。
        s.fold_event(&stream(2, 1, "新段的partial"));
        s.fold_event(&win_cal(1, "旧段定稿"));
        // 新段的 live 与首选不受旧段定稿影响。
        assert_eq!(s.live(), "新段的partial");
        assert_eq!(s.plain_preview(), "新段的partial");
        // 旧段定稿正常进 finals(顺序正确)。
        assert_eq!(s.finals(), vec!["旧段定稿"]);
    }

    /// **回归(round13 前端审计 F2)**:段关闭(PC)即清自己的 live → 首选
    /// 立即让位 finals,不把该段最后一句残留显示在首选上(与候选 2 重复)。
    #[test]
    fn paragraph_closed_clears_own_live_and_yields_preview() {
        let s = SharedTranscript::new();
        s.fold_event(&stream(1, 1, "最后一句流式"));
        s.fold_event(&AsrEvent::ParagraphClosed { paragraph_id: 1 });
        assert_eq!(s.live(), "", "PC 清自己段的 live");
        assert_eq!(s.plain_preview(), "", "首选让位 —— finals 承接");
        assert_eq!(s.finals(), vec!["最后一句流式"], "关闭即占位");
        // 之后新段开流,旧段的定稿到达也不影响新段 live(与上例互补的方向)。
        s.fold_event(&stream(2, 1, "新段"));
        s.fold_event(&win_cal(1, "定稿"));
        assert_eq!(s.live(), "新段");
        assert_eq!(s.plain_preview(), "新段");
        assert_eq!(s.finals(), vec!["定稿"]);
    }
}
