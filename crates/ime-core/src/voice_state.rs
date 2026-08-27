//! Shared voice-session state.
//!
//! 合并了原 [`AsrBuffer`](crate::asr_buffer::AsrBuffer) 的 candidate-surface API
//! (`set_live` / `push_final` / `voice_candidates` / `preview`) 与原 `AuraAgent`
//! 的 per-window recognition state(用于 `#asr/calc` 的预览)。
//!
//! ## 写入端:voice listener task
//!
//! 在 `IoThread` 的 runtime 上运行的 listener task 持有 `Arc<SharedVoiceState>`
//! 与 `AuraClient`。SSE 数据面到达 → `fold_segment(seg)` → 可选 `refresh_ui`。
//! **没有轮询**——所有变化都由 SSE 事件触发。
//!
//! ## 读取端:VoiceMember
//!
//! 魔法命令在主线程 (key event / tick) 同步读 `voice_state.voice_candidates()`
//! 和 `voice_state.preview()`。锁很短(微秒级字符串 clone)。
//!
//! ## Thread safety
//!
//! 纯 `std::sync` —— `Mutex` + `AtomicBool`。不依赖 tokio runtime,任何线程可读可写。
//!
//! ## 优雅降级(batch 不可靠、流式可靠)
//!
//! 流式(本地)是**底线**:live 只由 `StreamFragment` 写入;段/窗口的 batch
//! 文本在任何形式的缺席下(remote 端点掉线、识别失败、**迟到** —— remote
//! batch ~3.5s 可晚于 merge_gap 触发的窗口关闭)都逐级回退流式拼接。校准
//! (LLM)只增强 calc 预览与定稿,永不污染 live/plain。窗口关闭后 preview
//! 保留为 closed snapshot,迟到事件**刷新**它而不是清掉。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

/// Max retained settled utterances (candidate slots). 旧版本(`AsrBuffer`) 同值;
/// 保留以保持行为。
pub const MAX_FINALS: usize = 8;

/// 一个窗口的组装预览。`#asr` 候选首选项;`#asr/calc` 改用 `calc`。
///
/// - `plain`:`BatchWindow` > 逐段拼接(每段优先 `BatchSegment` > `StreamFragment`)
/// - `calc`:`WindowCalibration` > `BatchWindow` > `SegmentCalibration` > 逐段拼接
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AsrPreview {
    pub window_id: u64,
    pub plain: String,
    pub calc: String,
}

/// 一个 Segment 的识别状态(供 `WindowState` 折叠)。
#[derive(Debug, Clone, Default)]
struct SegmentState {
    /// 最近一次 `StreamFragment` 文本。
    stream: String,
    /// `BatchSegment` 结果(EOS 出 batch 后才有)。
    batch: Option<String>,
}

/// 一个窗口的识别状态。由 listener task 在收到 SSE 事件时折叠维护。
#[derive(Debug, Clone, Default)]
struct WindowState {
    /// 整窗已关闭(`BatchWindow` / `WindowCalibration` 到达)。
    closed: bool,
    /// 整窗 batch 重跑文本(`BatchWindow`,权威 raw)。
    batch_window: Option<String>,
    /// 定稿校准文本(`WindowCalibration`)。
    calibrated: Option<String>,
    /// Stage2 对当前窗口所有已到 Segment 的临时联合校准(`SegmentCalibration`)。
    segment_calibration: Option<String>,
    /// 各 Segment,按首见顺序。
    segments: Vec<(u64, SegmentState)>,
}

impl WindowState {
    /// 逐段拼接:每段有 `BatchSegment` 用 `BatchSegment`,否则 `StreamFragment`。
    fn concat_segments(&self) -> String {
        let mut out = String::new();
        for (_, seg) in &self.segments {
            let text = seg.batch.clone().unwrap_or_else(|| seg.stream.clone());
            out.push_str(&text);
        }
        out
    }

    /// 基本预览(plain):`BatchWindow`(整窗权威 batch)> 逐段拼接(每段
    /// `BatchSegment` > `StreamFragment`)。窗口未关闭恒为逐段拼接;关闭后
    /// `BatchWindow` 缺席(batch 失败 / disabled / 单段复用段级 None,后端
    /// 此时**不发事件**)也回退逐段拼接 —— 段级 batch/stream 文本仍在,不能
    /// 让首选候选闪没。
    fn plain_preview(&self) -> String {
        if self.closed {
            self.batch_window
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| self.concat_segments())
        } else {
            self.concat_segments()
        }
    }

    /// 校准优先预览(calc):窗口已关闭 → `WindowCalibration` 优先于 `BatchWindow`;
    /// 未关闭 → `SegmentCalibration` 优先于逐段拼接,识别中段用 `StreamFragment`。
    fn calc_preview(&self) -> String {
        if self.closed {
            return self
                .calibrated
                .clone()
                .or_else(|| self.batch_window.clone())
                .unwrap_or_default();
        }
        match &self.segment_calibration {
            Some(sc) if !sc.is_empty() => sc.clone(),
            _ => self.concat_segments(),
        }
    }
}

#[derive(Default)]
struct Inner {
    /// 当前流式文本(`StreamFragment` raw / `SegmentCalibration`)。`WindowCalibration`
    /// 后清空,等待下一窗口。
    live: String,
    /// 已定稿的句子,最新在前。`WindowCalibration` 时插入头部。
    finals: Vec<String>,
    /// 当前活动窗口的组装预览(`plain` / `calc`)。有活动窗口时刷新;窗口定稿
    /// 后保留最后快照(closed snapshot),迟到事件对它**重算增强**,直到下一
    /// 窗口首条流式替换。
    preview: Option<AsrPreview>,
    /// per-window 识别状态(`#asr/calc` 折叠源)。
    windows: HashMap<u64, WindowState>,
    /// 当前正在识别中的窗口 id(`StreamFragment` 维护,窗口定稿时清)。只做
    /// refresh 的定位键 —— 文本在各自的 `WindowState` 里,不在此冗余。
    current_window: Option<u64>,
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

/// 共享的语音会话状态。`Arc<SharedVoiceState>` 在 listener task 与 engine 之间共享。
pub struct SharedVoiceState {
    inner: Mutex<Inner>,
    /// Aura daemon 连通性三态(listener 的 health 探针 / SSE 流事件写入)。
    conn: AtomicU8,
}

impl SharedVoiceState {
    pub fn new() -> Self {
        SharedVoiceState {
            inner: Mutex::new(Inner::default()),
            conn: AtomicU8::new(VoiceConn::Connecting as u8),
        }
    }

    // ── Connectivity(三态)──────────────────────────────────────────────

    pub fn set_conn(&self, c: VoiceConn) {
        self.conn.store(c as u8, Ordering::Relaxed);
    }

    pub fn conn(&self) -> VoiceConn {
        match self.conn.load(Ordering::Relaxed) {
            1 => VoiceConn::Connected,
            2 => VoiceConn::Failed,
            _ => VoiceConn::Connecting,
        }
    }

    /// 兼容布尔判断:仅 `Connected` 视为"已连接"(TUI 等仍用)。
    pub fn is_connected(&self) -> bool {
        self.conn() == VoiceConn::Connected
    }

    // ── Sync setters(被 listener 与测试使用)──────────────────────────

    /// **清空全部历史**(finals / live / preview / windows)—— 重连后调用,避免
    /// 断连期间的旧句残留在候选里,导致 `#asr` 重新打开时首个候选不是当前句。
    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap();
        g.live.clear();
        g.finals.clear();
        g.preview = None;
        g.current_window = None;
        g.windows.clear();
    }

    /// 重连后**全量同步 aura 历史定稿**(`GET /api/results`,最旧 → 最新):
    /// 把每条非空 calibrated 灌入 finals(最新在前,截断 [`MAX_FINALS`])。
    /// 数据面 SSE 是 append-only,重连收不到断连期间的历史 —— 靠这里补齐。
    pub fn sync_history(&self, history: &[(u64, String)]) {
        let mut g = self.inner.lock().unwrap();
        g.finals.clear();
        g.live.clear();
        g.preview = None;
        g.current_window = None;
        g.windows.clear();
        // history 是最旧 → 最新;逐个头插 → 最新落在 finals[0](与"最新在前"
        // 语义一致)。
        for (_, calibrated) in history.iter() {
            if !calibrated.is_empty() {
                g.finals.insert(0, calibrated.clone());
                if g.finals.len() > MAX_FINALS {
                    g.finals.truncate(MAX_FINALS);
                }
            }
        }
    }

    /// 测试 / mock 用的流式文本注入(不更新 segments / preview,因为没有 window id
    /// 上下文)。生产代码永远走 `fold_segment`。
    pub fn set_live_raw(&self, text: &str) {
        let mut g = self.inner.lock().unwrap();
        g.live = text.to_string();
    }

    /// 测试 / mock 用的种子 final(对应旧 `AsrBuffer::update` 语义):
    /// 头部插入 + 截断,不等同于真实 `WindowCalibration` 的窗口关闭流程。
    pub fn seed_final(&self, text: &str) {
        let mut g = self.inner.lock().unwrap();
        if text.is_empty() {
            return;
        }
        g.finals.insert(0, text.to_string());
        if g.finals.len() > MAX_FINALS {
            g.finals.truncate(MAX_FINALS);
        }
        g.live.clear();
    }

    /// 显式写入 preview(由 listener 在收到 `live_window = Some(...)` 时根据窗口状态计算)。
    /// 当前 listener 路径在 `set_live` / `set_segment_calibration` 内部已经自动调用
    /// `refresh_preview_locked`,外部不需要直接调这个;保留作为公共 API。
    pub fn set_preview(&self, preview: AsrPreview) {
        let mut g = self.inner.lock().unwrap();
        if g.preview.as_ref() != Some(&preview) {
            g.preview = Some(preview);
        }
    }

    pub fn clear_preview(&self) {
        self.inner.lock().unwrap().preview = None;
    }

    // ── Sync getters ────────────────────────────────────────────────────

    /// `(finals most-recent-first, live)`。`#asr` 构建候选列表用。
    pub fn voice_candidates(&self) -> (Vec<String>, String) {
        let g = self.inner.lock().unwrap();
        (g.finals.clone(), g.live.clone())
    }

    /// 最近一次 preview(若无则 None)。
    pub fn preview(&self) -> Option<AsrPreview> {
        self.inner.lock().unwrap().preview.clone()
    }

    /// 旧的 `__ASR_BUFFER__` 展开用:最新 final > live > ""。
    pub fn snapshot(&self) -> String {
        let g = self.inner.lock().unwrap();
        g.finals
            .first()
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| g.live.clone())
    }

    // ── Listener fold(由 IoThread 上的 voice listener task 调用)────────

    /// 折叠一个 `AsrSegment` 到内部状态。listener 在 SSE 数据面到来时调。
    pub fn fold_segment(&self, seg: &audio_aura_agent::view::AsrSegment) {
        let mut g = self.inner.lock().unwrap();
        match seg {
            audio_aura_agent::view::AsrSegment::StreamFragment {
                window_id,
                segment_id,
                text,
                ..
            } => {
                g.live = text.clone();
                g.current_window = Some(*window_id);
                let entry = g.windows.entry(*window_id).or_default();
                if !entry.closed {
                    upsert_segment(&mut entry.segments, *segment_id, |s| {
                        s.stream = text.clone();
                    });
                }
                Self::refresh_preview_locked(&mut g);
            }
            audio_aura_agent::view::AsrSegment::BatchSegment {
                window_id,
                segment_id,
                text,
            } => {
                let entry = g.windows.entry(*window_id).or_default();
                upsert_segment(&mut entry.segments, *segment_id, |s| {
                    s.batch = Some(text.clone());
                });
                Self::refresh_preview_locked(&mut g);
            }
            audio_aura_agent::view::AsrSegment::BatchWindow { window_id, text } => {
                let entry = g.windows.entry(*window_id).or_default();
                entry.closed = true;
                entry.batch_window = Some(text.clone());
                Self::refresh_preview_locked(&mut g);
            }
            audio_aura_agent::view::AsrSegment::SegmentCalibration {
                window_id,
                calibrated,
            } => {
                // 校准不写 `live` —— live 是流式底线(本地、可靠),迟到的校准
                // 文本把 live 倒退回旧句会造成显示抖动。校准只进窗口状态,
                // 供 calc 预览与窗口定稿存档。
                let entry = g.windows.entry(*window_id).or_default();
                entry.segment_calibration = Some(calibrated.clone());
                Self::refresh_preview_locked(&mut g);
            }
            audio_aura_agent::view::AsrSegment::WindowCalibration {
                window_id,
                calibrated,
            } => {
                if !calibrated.is_empty() {
                    g.finals.insert(0, calibrated.clone());
                    if g.finals.len() > MAX_FINALS {
                        g.finals.truncate(MAX_FINALS);
                    }
                }
                g.live.clear();
                // 窗口已关闭。preview 仍保留(closed snapshot),calc = 定稿(用户
                // 选 /calc 时看);plain = BatchWindow > 逐段拼接。current_window
                // 清空 —— 下一条流式(下一窗口)到来时 preview 才切换目标。
                g.current_window = None;
                let entry = g.windows.entry(*window_id).or_default();
                entry.closed = true;
                entry.calibrated = Some(calibrated.clone());
                let plain = entry.plain_preview();
                let calc = entry.calc_preview();
                g.preview = Some(AsrPreview {
                    window_id: *window_id,
                    plain,
                    calc,
                });
            }
            audio_aura_agent::view::AsrSegment::Correction { .. } => {
                // Stage2 correction feedback 暂不处理(后续通过 AuraClient::correct 单独触发)。
            }
        }
    }

    /// 根据 `current_window` + 对应 `WindowState` 重新计算并写入 preview。
    ///
    /// 无活动窗口(窗口定稿后、下一窗口首条流式前)时,按最近 preview 记录的
    /// 窗口**重算增强**而非清空 —— remote batch(~3.5s)+ LLM 可晚于
    /// merge_gap 触发的窗口关闭,迟到的 `BatchSegment` / `SegmentCalibration`
    /// 仍会触发 refresh;清空会把首选候选(🎙 组装预览)闪没。清空只由
    /// `reset` / `sync_history` / `clear_preview` 显式做。
    /// 调用者必须已持有 inner 锁。
    fn refresh_preview_locked(g: &mut Inner) {
        let Some(&window_id) = g
            .current_window
            .as_ref()
            .or_else(|| g.preview.as_ref().map(|p| &p.window_id))
        else {
            return;
        };
        let Some(win) = g.windows.get(&window_id) else {
            return;
        };
        g.preview = Some(AsrPreview {
            window_id,
            plain: win.plain_preview(),
            calc: win.calc_preview(),
        });
    }
}

impl Default for SharedVoiceState {
    fn default() -> Self {
        Self::new()
    }
}

fn upsert_segment(
    segments: &mut Vec<(u64, SegmentState)>,
    segment_id: u64,
    f: impl FnOnce(&mut SegmentState),
) {
    if let Some((_, s)) = segments.iter_mut().find(|(id, _)| *id == segment_id) {
        f(s);
    } else {
        segments.push((segment_id, SegmentState::default()));
        if let Some((_, s)) = segments.last_mut() {
            f(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_aura_agent::view::AsrSegment;

    fn stream(wid: u64, sid: u64, text: &str) -> AsrSegment {
        AsrSegment::StreamFragment {
            window_id: wid,
            segment_id: sid,
            text: text.into(),
            at_s: 0.0,
        }
    }

    fn batch_seg(wid: u64, sid: u64, text: &str) -> AsrSegment {
        AsrSegment::BatchSegment {
            window_id: wid,
            segment_id: sid,
            text: text.into(),
        }
    }

    fn batch_window(wid: u64, text: &str) -> AsrSegment {
        AsrSegment::BatchWindow {
            window_id: wid,
            text: text.into(),
        }
    }

    fn seg_cal(wid: u64, text: &str) -> AsrSegment {
        AsrSegment::SegmentCalibration {
            window_id: wid,
            calibrated: text.into(),
        }
    }

    fn win_cal(wid: u64, text: &str) -> AsrSegment {
        AsrSegment::WindowCalibration {
            window_id: wid,
            calibrated: text.into(),
        }
    }

    #[test]
    fn stream_then_window_final_pushes_final_clears_live() {
        let s = SharedVoiceState::new();
        s.fold_segment(&stream(1, 1, "ni"));
        s.fold_segment(&batch_seg(1, 1, "你好"));
        s.fold_segment(&win_cal(1, "你好世界"));
        let (finals, live) = s.voice_candidates();
        assert_eq!(finals, vec!["你好世界"]);
        assert_eq!(live, "");
        // 窗口关闭后 preview 保留(calc = 定稿),不再有 live 预览刷新。
        let p = s.preview().expect("preview persists as closed snapshot");
        assert_eq!(p.window_id, 1);
        assert_eq!(p.calc, "你好世界");
    }

    #[test]
    fn open_window_preview_concats_segments_in_order() {
        let s = SharedVoiceState::new();
        s.fold_segment(&stream(1, 1, "第一段"));
        s.fold_segment(&batch_seg(1, 1, "第一段"));
        s.fold_segment(&stream(1, 2, "第二段"));
        let p = s.preview().expect("preview after stream+batch+stream");
        assert_eq!(p.plain, "第一段第二段");
    }

    #[test]
    fn closed_window_preview_uses_batch_window() {
        let s = SharedVoiceState::new();
        s.fold_segment(&stream(1, 1, "raw"));
        s.fold_segment(&batch_seg(1, 1, "raw"));
        s.fold_segment(&batch_window(1, "整窗batch"));
        let p = s.preview().expect("preview after batch_window");
        assert_eq!(p.plain, "整窗batch");
        assert_eq!(p.calc, "整窗batch");
    }

    #[test]
    fn calc_prefers_window_calibration_over_batch_window() {
        let s = SharedVoiceState::new();
        s.fold_segment(&stream(1, 1, "raw"));
        s.fold_segment(&batch_seg(1, 1, "raw"));
        s.fold_segment(&batch_window(1, "整窗batch"));
        s.fold_segment(&win_cal(1, "定稿"));
        let p = s.preview().expect("preview after window_calibration");
        assert_eq!(p.calc, "定稿");
    }

    #[test]
    fn calc_open_prefers_segment_calibration_over_concat() {
        let s = SharedVoiceState::new();
        s.fold_segment(&stream(1, 1, "第一段"));
        s.fold_segment(&batch_seg(1, 1, "第一段"));
        s.fold_segment(&stream(1, 2, "第二段"));
        // 阶段2 联合校准到达 → calc 走它。
        s.fold_segment(&seg_cal(1, "联合整流"));
        let p = s.preview().expect("preview after segment_calibration");
        assert_eq!(p.calc, "联合整流");
        // 仍未关闭,plain 走 concat(第一段 batch, 第二段 stream)。
        assert_eq!(p.plain, "第一段第二段");
    }

    #[test]
    fn finals_capped_newest_first() {
        let s = SharedVoiceState::new();
        for i in 0..(MAX_FINALS + 3) as u8 {
            s.fold_segment(&win_cal(1, &format!("句{i}")));
        }
        let (finals, _) = s.voice_candidates();
        assert_eq!(finals.len(), MAX_FINALS);
        assert_eq!(finals[0], format!("句{}", MAX_FINALS + 2));
        assert_eq!(finals.last().unwrap(), "句3");
    }

    #[test]
    fn snapshot_prefers_latest_final() {
        let s = SharedVoiceState::new();
        s.fold_segment(&stream(1, 1, "流式中"));
        s.fold_segment(&win_cal(1, "定稿一"));
        s.fold_segment(&stream(1, 2, "流式二"));
        assert_eq!(s.snapshot(), "定稿一");
    }

    #[test]
    fn connectivity_isolated_from_data() {
        let s = SharedVoiceState::new();
        assert_eq!(s.conn(), VoiceConn::Connecting);
        s.set_conn(VoiceConn::Connected);
        assert!(s.is_connected());
        assert_eq!(s.conn(), VoiceConn::Connected);
        s.set_conn(VoiceConn::Failed);
        assert!(!s.is_connected());
        assert_eq!(s.conn(), VoiceConn::Failed);
    }

    #[test]
    fn window_calibration_empty_text_does_not_push_final() {
        let s = SharedVoiceState::new();
        s.fold_segment(&stream(1, 1, "hi"));
        s.fold_segment(&win_cal(1, ""));
        let (finals, _) = s.voice_candidates();
        assert!(finals.is_empty());
    }
}
