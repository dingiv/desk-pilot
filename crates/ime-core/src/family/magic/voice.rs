//! VoiceMember — `#asr`: voice prediction provider.
//!
//! 纯读路径:voice server 在引擎 I/O 线程上后台拉 SSE,折叠 AsrSegment 到
//! [`SharedVoiceState`](crate::voice_state::SharedVoiceState)。本成员只读这个
//! shared state 来产生候选 —— 不再有任何轮询 / 异步状态。
//!
//! 预测模型下,`#asr` 精确匹配时提供语音结果预测(最多 4 条:流式 live +
//! 已定稿 finals),放候选列表最前,`#asr` 本身是末尾 rollback。
//!
//! ## 与 voice server 的协作(懒惰,按需)
//!
//! **每次 `predict` 都发 [`VoiceCmd::Attach`]** 把当前 ctx 推给 IoThread 上的
//! voice server —— 它此刻才去检查/建立 aura 连接,并立即刷一次 UI;之后每收
//! 到一个 SSE 段就顺带刷新(目标 ctx)。会话结束由 `deactivate` 发
//! [`VoiceCmd::Detach`],而 voice server 自己也会在 `refresh_ui` 失败时"轻易
//! 放弃"。成员**不维护任何停止状态**。
//!
//! 参数(路径/查询)调整预测:
//! - `?num=N` → 预测 = [语音队列最新 N 条定稿拼接](选中即上屏);
//! - `/calc` → 预测 = [校准优先预览](由 listener 折叠)
//! - 其余路径(/en 翻译等)留白,预测仍是语音结果。

use std::sync::Arc;

use super::member::{CommandArgs, MagicMember, Prediction};
use super::MagicResources;
use crate::io_thread::{VoiceCmd, VoiceCmdSender};
use crate::state::{StateMachine, StepEnv};
use crate::voice_state::SharedVoiceState;

const MAX_SUBMIT: usize = 4;

/// Live voice-input command (`#asr`)。
pub struct VoiceMember {
    resources: Arc<MagicResources>,
}

impl VoiceMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        VoiceMember { resources }
    }

    /// 取共享 voice state(未注入时 None —— 测试场景)。
    fn state(&self) -> Option<Arc<SharedVoiceState>> {
        self.resources.voice_state()
    }

    /// 取 voice server 命令 sender。
    fn tx(&self) -> Option<VoiceCmdSender> {
        self.resources.voice_tx()
    }

    /// 触发名之后的参数串(`#asr/en?num=2` → `/en?num=2`)。
    fn args_of(input: &str) -> CommandArgs {
        let rest = input
            .strip_prefix('#')
            .and_then(|r| r.strip_prefix("asr"))
            .unwrap_or("");
        CommandArgs::parse(rest)
    }

    /// 提交语音队列里最新的 `n` 条定稿(换行拼接)。不足 `n` 条提交现有。
    fn commit_last_n(&self, n: usize) -> Option<String> {
        let state = self.state()?;
        let (finals, _) = state.voice_candidates();
        if finals.is_empty() {
            return None;
        }
        let take = n.min(finals.len());
        Some(finals[..take].join("\n"))
    }

    /// 语音结果预测:流式 live(有则)在前,定稿次之,最多 4 条。
    fn voice_predictions(&self, state: &SharedVoiceState) -> Vec<Prediction> {
        if !state.is_connected() {
            return Vec::new();
        }
        let (finals, live) = state.voice_candidates();
        let mut out = Vec::new();
        // 首选 = 当前窗口的组装预览:小停顿产生新 batch 后继续说话,预览是
        // 前段 Batch + 当前段流式("第一句第二句"),而不是只有当前段流式。
        // 无预览时回退到原始 live。
        let composed = state
            .preview()
            .map(|p| p.plain)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| live.clone());
        if !composed.is_empty() {
            out.push(Prediction::commit_raw(format!("🎙 {}", composed), composed));
        }
        for f in finals.iter().take(MAX_SUBMIT - out.len()) {
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

    fn predict(&mut self, ctx: usize, input: &str, env: &dyn StepEnv) -> Vec<Prediction> {
        // 每次预测都通知 voice server 这个 ctx —— 幂等,懒 server 借此重瞄
        // 目标 / 重连 / 立即刷一次 UI。发不到(未接线)就静默,预测照常。
        match env.voice_cmd_tx().or_else(|| self.tx()) {
            Some(tx) => {
                tx.send(VoiceCmd::Attach { ctx });
                tracing::info!(ctx, "asr predict → Attach{ctx}");
            }
            None => tracing::warn!(ctx, "asr predict: 无 voice_tx(未接线?)"),
        }

        let Some(state) = self.state() else {
            return vec![Prediction::interactive("voice listener 未启动")];
        };
        let args = Self::args_of(input);

        // 未连接:不可提交的解释(interactive = 选中不上屏)。
        if !state.is_connected() {
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
            if let Some(p) = state.preview() {
                if !p.calc.is_empty() {
                    return vec![Prediction::commit(p.calc)];
                }
            }
        }

        let out = self.voice_predictions(&state);
        if out.is_empty() {
            // 连接但暂无语音:🎙 占位(选中不上屏)。正常到不了(voice_predictions
            // 已含 🎙),兜底用。
            vec![Prediction::interactive("🎙")]
        } else {
            out
        }
    }

    fn tick(&mut self, _sm: &mut StateMachine, _env: &dyn StepEnv) -> Option<Vec<Prediction>> {
        // 数据变化由 voice server 主动调 `frontend.refresh_ui` 触发 —— 我们的
        // predict 已经把最新 state 算成 candidates,无需 tick 路径重建。
        None
    }

    fn deactivate(&mut self, ctx: usize) {
        // 让 voice server 尽早放弃(它也会在 refresh 失败时自愈)。
        if let Some(tx) = self.tx() {
            tx.send(VoiceCmd::Detach { ctx });
        }
    }
}
