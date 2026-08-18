//! VoiceMember — `#asr`: live voice-input anchor.
//!
//! Activation token `__ASR_BUFFER__`. The candidate list tracks the aura stream:
//! the live interim as #1 while streaming, then settled finals (newest first).
//! Space commits the **full** #1 text; Esc / Enter / Backspace cancel; other keys
//! pass through so typing while listening still works. `tick` rebuilds the view
//! when the shared [`AsrBuffer`] version advances — the async-refresh timer makes
//! the old `#flush` alias (manual refresh) unnecessary.

use std::sync::Arc;

use super::member::{is_arg_char, CommandArgs, MagicMember, MemberAction};
use super::MagicResources;
use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};

/// Live voice-input command (`#asr` / `#flush`).
pub struct VoiceMember {
    /// Shared resources — the voice buffer slot is attached late (after engine
    /// construction), so it lives behind an `Arc` shared with the engine.
    resources: Arc<MagicResources>,
    /// Last `AsrBuffer::version()` seen — `tick` compares to detect changes.
    last_version: u64,
    /// Last connectivity state — `tick` rebuilds when it flips (a disconnect or
    /// reconnect doesn't touch the version counter).
    last_connected: bool,
    /// Candidate texts (same as `sm.candidates` — full, un-truncated; each
    /// frontend truncates rows for its own display). Space commits `full[0]`.
    full: Vec<String>,
    /// Aura is known-unavailable (no buffer attached, or reported disconnected).
    /// The preedit/candidate explain instead of pretending to recognize, and
    /// Space commits nothing.
    unavailable: bool,
    /// 路径/查询参数的原始串(`/en`、`?num=2` …),命令触发后逐键积累。
    arg: String,
    /// `arg` 的解析结果,每次积累后重新解析 —— 家族内部据此路由。
    args: CommandArgs,
}

impl VoiceMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        VoiceMember {
            resources,
            last_version: 0,
            last_connected: false,
            full: Vec::new(),
            unavailable: false,
            arg: String::new(),
            args: CommandArgs::default(),
        }
    }

    /// 参数的可读标记(翻译模式 / 校准预览 / 未知参数 / num 提交)。空参数字符串时为空。
    fn arg_marker(&self) -> String {
        if self.arg.is_empty() { return String::new(); }
        let mut s = String::new();
        if self.args.has_path("calc") {
            s.push_str(" · 校准预览");
        }
        if self.args.has_path("en") {
            s.push_str(" · 英文翻译(待实现)");
        } else if !self.args.path.is_empty() && !self.args.has_path("calc") {
            s.push_str(" · 未知参数");
        }
        if self.args.get("num").is_some() {
            s.push_str(" · 提交最近 N 条");
        }
        s
    }

    /// 提交语音队列里最新的 `n` 条定稿(换行拼接)。不足 `n` 条提交现有。
    fn commit_last_n(&self, n: usize) -> Option<String> {
        let buf = self.resources.voice.get()?;
        let (finals, _) = buf.voice_candidates();
        if finals.is_empty() { return None; }
        let take = n.min(finals.len());
        Some(finals[..take].join("\n"))
    }

    /// 参数积累后的处理:重新解析并路由。`?num=N` 立即提交最新 N 条;
    /// 其余参数只重建视图(翻译/未知参数等具体语义留待实现)。
    fn apply_args(&mut self, sm: &mut StateMachine) -> MemberAction {
        self.args = CommandArgs::parse(&self.arg);
        if let Some(raw) = self.args.get("num") {
            if let Ok(n) = raw.parse::<usize>() {
                if n > 0 {
                    if let Some(text) = self.commit_last_n(n) {
                        return MemberAction::Commit(text);
                    }
                }
            }
        }
        MemberAction::View(Box::new(self.refresh(sm)))
    }

    /// 是否启用校准优先预览(`#asr/calc`)。
    fn use_calc_preview(&self) -> bool {
        self.args.has_path("calc")
    }

    /// Rebuild the candidate view from the voice buffer: `[window-preview, finals…]`
    /// — the current window's composed preview is #1, then settled finals
    /// newest→oldest. The preview comes from aura-agent's `get_window_preview`
    /// (plain) / `get_window_calc_preview`(`/calc`,校准优先)经 bridge 喂入
    /// `AsrBuffer`;无预览时回退到原始 `live`(StreamFragment/SegmentCalibration)。
    /// `sm.candidates` holds previews; `self.full` holds the full texts for commit.
    ///
    /// When aura is NOT connected (or no buffer is attached at all), the preedit
    /// says so explicitly instead of the normal "🎙 #asr …" — the user learns the
    /// voice path is down, and Space commits nothing.
    fn refresh(&mut self, sm: &mut StateMachine) -> ImeView {
        let buf = self.resources.voice.get();
        let connected = buf.as_ref().map(|b| b.is_connected()).unwrap_or(false);
        self.last_connected = connected;

        let mut full: Vec<String> = Vec::new();
        if connected {
            if let Some(buf) = buf.as_ref() {
                let (finals, live) = buf.voice_candidates();
                // 组装预览优先(bridge 喂入的当前窗口组装文本);无预览则回退
                // 原始流式 live。
                let preview = buf.preview();
                let composed = match &preview {
                    Some(p) if self.use_calc_preview() && !p.calc.is_empty() => Some(p.calc.clone()),
                    Some(p) if !self.use_calc_preview() && !p.plain.is_empty() => Some(p.plain.clone()),
                    _ => None,
                };
                match composed {
                    Some(text) => full.push(text),
                    None => {
                        if !live.is_empty() {
                            full.push(live); // active streaming → #1
                        }
                    }
                }
                full.extend(finals); // then settled, newest→oldest
            }
        }
        let empty = full.is_empty();
        self.full = full;
        self.unavailable = !connected;

        sm.candidates_fresh = true;
        sm.candidate_highlight = 0;
        sm.candidate_page = 0;
        let trigger = format!("#{}{}{}", self.name(), self.arg, self.arg_marker());
        match (connected, empty) {
            // Aura down / never connected: explain in the preedit, non-committable
            // candidate (Space commits nothing — the same guard as the placeholder).
            (false, _) => {
                sm.candidates = vec!["aura 未连接，语音不可用".to_string()];
                sm.preedit = format!("🎙 {trigger} — aura 未连接，语音不可用");
            }
            // Connected but no voice data yet — placeholder until it arrives.
            (true, true) => {
                sm.candidates = vec!["语音识别中...".to_string()];
                self.full.clear();
                sm.preedit = format!("🎙 {trigger} …");
            }
            // Connected with data: full texts, un-truncated — each frontend renders
            // rows to its own space (the TUI has room and shows everything; the
            // fcitx5 panel truncates at its export). Commit uses `self.full`, so
            // display never loses data. The preedit expands the anchor into the
            // recognized text — the preedit slot is large (512 B) and frontends
            // show more of it than the 6-char rows, so the user reads the sentence
            // while it streams in.
            (true, false) => {
                sm.candidates = self.full.clone();
                let head = self.full.first().cloned().unwrap_or_default();
                sm.preedit = format!("🎙 {trigger} {head}");
            }
        }
        sm.cursor = sm.preedit.len();
        if let Some(b) = buf.as_ref() {
            self.last_version = b.version();
        }
        tracing::debug!(candidates = ?sm.candidates, connected, "voice candidates rebuilt");
        sm.make_view()
    }
}

impl MagicMember for VoiceMember {
    fn name(&self) -> &'static str {
        "asr"
    }

    fn description(&self) -> &'static str {
        "voice input"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__ASR_BUFFER__")
    }

    fn aliases(&self) -> &[&'static str] {
        &[] // #flush removed — async magic tick refreshes the view automatically
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(VoiceMember::new(Arc::clone(&self.resources)))
    }

    fn activate(&mut self, sm: &mut StateMachine, _env: &dyn StepEnv) -> ImeView {
        self.refresh(sm)
    }

    fn on_key(&mut self, sm: &mut StateMachine, ch: char, _env: &dyn StepEnv) -> MemberAction {
        // ── 路径/查询参数积累 ──
        // `/` 或 `?` 开启参数;开启后字母/数字/`=`/`&` 等都是参数字符,一路
        // 积累到终止键(Space/Enter/Esc/Backspace)。`?num=N` 会立即提交
        // 最新 N 条定稿。
        if self.arg.is_empty() && (ch == '/' || ch == '?') {
            self.arg.push(ch);
            return MemberAction::View(Box::new(self.refresh(sm)));
        }
        if !self.arg.is_empty() && is_arg_char(ch) {
            self.arg.push(ch);
            return self.apply_args(sm);
        }

        match ch {
            ' ' => {
                // Aura unavailable: never commit the "语音不可用" explainer text.
                if self.unavailable {
                    return MemberAction::Commit(String::new());
                }
                // Commit the FULL text (self.full), not the display preview (sm.candidates).
                let text = self.full.first().cloned()
                    .unwrap_or_else(|| sm.candidates.first().cloned().unwrap_or_default());
                // Placeholder ("…"/"语音识别中...") or empty → commit nothing.
                if text.is_empty() || text.ends_with("...") {
                    MemberAction::Commit(String::new())
                } else {
                    MemberAction::Commit(text)
                }
            }
            d @ '1'..='9' => {
                let idx = (d as u8 - b'1') as usize;
                match self.full.get(idx) {
                    Some(t) => MemberAction::Commit(t.clone()),
                    // No such candidate — let the application have the digit.
                    None => MemberAction::View(Box::new(StateMachine::passthrough_view())),
                }
            }
            // Enter: force-commit the trigger text (`#asr`) and exit the session —
            // "回车强制 '#asr' 上屏". Esc/Backspace still cancel.
            '\n' | '\r' => MemberAction::Commit(format!("#{}", self.name())),
            '\x1b' => MemberAction::Exit,
            '\x08' => {
                // Backspace: 先删参数,删空后退回取消整个会话。
                if !self.arg.is_empty() {
                    self.arg.pop();
                    self.args = CommandArgs::parse(&self.arg);
                    MemberAction::View(Box::new(self.refresh(sm)))
                } else {
                    MemberAction::Exit
                }
            }
            _ => MemberAction::View(Box::new(StateMachine::passthrough_view())),
        }
    }

    fn tick(&mut self, sm: &mut StateMachine, _env: &dyn StepEnv) -> Option<ImeView> {
        let buf = self.resources.voice.get()?;
        let cur = buf.version();
        let connected = buf.is_connected();
        if cur == self.last_version && connected == self.last_connected {
            // Voice data hasn't advanced since the last rebuild (normal during
            // silence), and connectivity hasn't flipped (a disconnect/reconnect
            // doesn't touch the version counter). Trace-level to avoid spam —
            // the timer repeats.
            tracing::trace!(last_version = self.last_version, cur, "voice tick: no version change");
            return None;
        }
        tracing::debug!(last_version = self.last_version, cur, connected, "voice tick rebuild");
        Some(self.refresh(sm))
    }

    fn candidate_texts(&self, sm: &StateMachine) -> Vec<String> {
        if self.full.is_empty() {
            sm.candidates.clone()
        } else {
            self.full.clone()
        }
    }
}

/// `#submit` — one-shot voice snapshot commit. Activates to a single candidate
/// (the latest settled utterance, or a hint if none); Space commits it, Escape
/// cancels.
pub struct SubmitMember {
    resources: Arc<MagicResources>,
}

impl SubmitMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        SubmitMember { resources }
    }
}

impl MagicMember for SubmitMember {
    fn name(&self) -> &'static str {
        "submit"
    }

    fn description(&self) -> &'static str {
        "commit voice snapshot"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__ASR_SUBMIT__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(SubmitMember::new(Arc::clone(&self.resources)))
    }

    fn activate(&mut self, sm: &mut StateMachine, _env: &dyn StepEnv) -> ImeView {
        let text = self.resources.voice.get().map(|b| b.snapshot()).unwrap_or_default();
        sm.candidates = vec![if text.is_empty() { "无语音内容".into() } else { text }];
        sm.candidates_fresh = true;
        sm.candidate_highlight = 0;
        sm.candidate_page = 0;
        sm.preedit = "#submit".into();
        sm.cursor = sm.preedit.len();
        sm.make_view()
    }

    fn on_key(&mut self, sm: &mut StateMachine, ch: char, _env: &dyn StepEnv) -> MemberAction {
        match ch {
            ' ' => {
                let text = sm.candidates.first().cloned().unwrap_or_default();
                if text == "无语音内容" {
                    MemberAction::Commit(String::new())
                } else {
                    MemberAction::Commit(text)
                }
            }
            // Enter: force-commit the trigger text (`#submit`) and exit. Esc/Backspace cancel.
            '\n' | '\r' => MemberAction::Commit(format!("#{}", self.name())),
            '\x1b' | '\x08' => MemberAction::Exit,
            _ => MemberAction::View(Box::new(StateMachine::passthrough_view())),
        }
    }

    fn tick(&mut self, _sm: &mut StateMachine, _env: &dyn StepEnv) -> Option<ImeView> {
        None
    }
}
