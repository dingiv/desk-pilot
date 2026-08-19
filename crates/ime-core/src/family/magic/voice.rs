//! VoiceMember — `#asr`: voice prediction provider.
//!
//! 预测模型下,`#asr` 精确匹配时提供语音结果预测(最多 4 条:流式 live +
//! 已定稿 finals),放候选列表最前,`#asr` 本身是末尾 rollback。
//!
//! 参数(路径/查询)调整预测:
//! - `?num=N` → 预测 = [语音队列最新 N 条定稿拼接](选中即上屏);
//! - `/calc` → 预测 = [校准优先预览](get_window_calc_preview 经 bridge 喂入);
//! - 其余路径(/en 翻译等)留白,预测仍是语音结果。
//!
//! `tick` 在 AsrBuffer 版本变化时重新预测(voice 流式/预览更新)。

use std::sync::Arc;

use super::member::{CommandArgs, MagicMember, Prediction};
use super::MagicResources;
use crate::state::{StateMachine, StepEnv};

/// Live voice-input command (`#asr`)。
pub struct VoiceMember {
    /// Shared resources — the voice buffer slot is attached late (after engine
    /// construction), so it lives behind an `Arc` shared with the engine.
    resources: Arc<MagicResources>,
    /// Last `AsrBuffer::version()` seen — `tick` compares to detect changes.
    last_version: u64,
    /// Last connectivity — 断开/重连不碰 version 计数,单独追踪。
    last_connected: bool,
}

impl VoiceMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        VoiceMember { resources, last_version: 0, last_connected: false }
    }

    /// 触发名之后的参数串(`#asr/en?num=2` → `/en?num=2`)。
    fn args_of(input: &str) -> CommandArgs {
        let rest = input.strip_prefix('#').and_then(|r| r.strip_prefix("asr")).unwrap_or("");
        CommandArgs::parse(rest)
    }

    /// 提交语音队列里最新的 `n` 条定稿(换行拼接)。不足 `n` 条提交现有。
    fn commit_last_n(&self, n: usize) -> Option<String> {
        let buf = self.resources.voice.get()?;
        let (finals, _) = buf.voice_candidates();
        if finals.is_empty() { return None; }
        let take = n.min(finals.len());
        Some(finals[..take].join("\n"))
    }

    /// 语音结果预测:流式 live(有则)在前,定稿次之,最多 4 条。
    fn voice_predictions(&self) -> Vec<Prediction> {
        let buf = match self.resources.voice.get() {
            Some(b) if b.is_connected() => b,
            _ => return Vec::new(),
        };
        let (finals, live) = buf.voice_candidates();
        let mut out = Vec::new();
        if !live.is_empty() {
            out.push(Prediction::commit(live));
        }
        for f in finals.iter().take(4 - out.len()) {
            out.push(Prediction::commit(f.clone()));
        }
        out
    }
}

impl MagicMember for VoiceMember {
    fn name(&self) -> &'static str {
        "asr"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__ASR_BUFFER__")
    }

    fn aliases(&self) -> &[&'static str] {
        &[]
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(VoiceMember::new(Arc::clone(&self.resources)))
    }

    fn predict(&mut self, input: &str, _env: &dyn StepEnv) -> Vec<Prediction> {
        let args = Self::args_of(input);
        let connected = self.resources.voice.get()
            .map(|b| b.is_connected())
            .unwrap_or(false);

        // 未连接:不可提交的解释(interactive = 选中不上屏)。
        if !connected {
            return vec![Prediction::interactive("aura 未连接，语音不可用")];
        }

        // `?num=N` → 预测 = 最近 N 条定稿拼接。
        if let Some(raw) = args.get("num") {
            if let Ok(n) = raw.parse::<usize>() {
                if n > 0 {
                    if let Some(text) = self.commit_last_n(n) {
                        return vec![Prediction::commit(text)];
                    }
                }
            }
        }

        // `/calc` → 校准优先预览。
        if args.has_path("calc") {
            if let Some(p) = self.resources.voice.get().as_ref().and_then(|b| b.preview()) {
                if !p.calc.is_empty() {
                    return vec![Prediction::commit(p.calc)];
                }
            }
        }

        // 消费当前版本与连接性 —— tick 只对之后的语音/连接变化触发重建。
        if let Some(b) = self.resources.voice.get() {
            self.last_version = b.version();
            self.last_connected = b.is_connected();
        }
        let out = self.voice_predictions();
        if out.is_empty() {
            // 连接但暂无语音:占位(选中不上屏)。
            vec![Prediction::interactive("语音识别中...")]
        } else {
            out
        }
    }

    fn tick(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> Option<Vec<Prediction>> {
        let buf = self.resources.voice.get()?;
        let cur = buf.version();
        let connected = buf.is_connected();
        if cur == self.last_version && connected == self.last_connected {
            return None; // voice 数据 / 预览 / 连接性都没变
        }
        self.last_version = cur;
        self.last_connected = connected;
        let input = sm.buffer.clone();
        Some(self.predict(&input, env))
    }
}

/// `#submit` — one-shot voice snapshot. 预测 = [最新定稿或提示](选中即上屏)。
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

    fn activation_token(&self) -> Option<&'static str> {
        Some("__ASR_SUBMIT__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(SubmitMember::new(Arc::clone(&self.resources)))
    }

    fn predict(&mut self, _input: &str, _env: &dyn StepEnv) -> Vec<Prediction> {
        let text = self.resources.voice.get().map(|b| b.snapshot()).unwrap_or_default();
        if text.is_empty() {
            vec![Prediction::interactive("无语音内容")]
        } else {
            vec![Prediction::commit(text)]
        }
    }
}
