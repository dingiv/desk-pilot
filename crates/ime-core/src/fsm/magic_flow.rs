//! magic — 第二路键处理:MagicFlow(魔法命令双数据流,round13)。
//!
//! **同步数据流**:`#` 前缀触发(Idle 输 `#` 进 Snippet,`X'#cmd` 链式)——
//! 命令查询(精确/前缀/参数/未知)、候选选中(预测提交/补全改写/rollback)、
//! 裸输入强触发。
//!
//! **异步数据流**:命令触发后台动作(`#asr` 语音、`#req` HTTP)后,由
//! 前台 IoThread 在完成时注入 `ImeEvent::Async(MagicTick)` —— 驱动
//! Magic 状态机转移(`magic_tick`),重建面板视图。
//!
//! 本模块**不依赖 ControlPane** —— 只吃 [`SessionState`](会话纯数据);
//! 家族级资源(voice 槽/req 配置/snippets/expander)由引擎持有的
//! MagicFamily 经 [`StepEnv`] 注入,成员实例(每会话一份)在
//! [`MagicSession`]。
//!

use crate::family::magic::{
    ChainContext, LiveCommand, MagicMatch, MagicMember, Prediction,
};

use crate::frontend::ImeView;
use super::family_prediction::{ComposeState, StepEnv};
use super::post::{commit_view, commit_view_at, delete_view};
use super::control::SessionState;

/// 魔法命令会话(S4 状态下沉):snippet 态(`#…`)的会话状态。
/// hints/predictions 的候选语义与 CandidatePanel 分离 —— 命令候选不是
/// scorer 产物(Matcher→Expander 路径),提交/改写规则也不同。
#[derive(Default)]
pub(crate) struct MagicSession {
    /// 补全提示(输入是某命令触发串的严格前缀):候选 = [补全名…, rollback]。
    /// 选中补全名 → **改写输入**(不提交)。
    pub hints: Vec<String>,
    /// 精确匹配命令时的预测选项(不含 rollback)。
    pub predictions: Vec<crate::family::magic::Prediction>,
    /// 当前精确匹配的 live 命令实例(保 req 异步态等);静态命令 / 前缀 /
    /// 未知时为 None。
    pub active: Option<Box<dyn MagicMember>>,
    /// 数字键是否用于选中候选(精确无参 / 前缀时 true;拼参数时 false)。
    pub selectable: bool,
}

impl SessionState {
    // ── Magic command prediction (Snippet state) ────────────────────

    /// 每次字符变化后重查:精确匹配 → 命令预测;前缀 → 补全提示;未知 → raw。
    pub(crate) fn query_magic(&mut self, env: &dyn StepEnv) -> ImeView {
        let input = self.comp.buffer.clone();

        // ── 链式命令模式(X'#cmd):上游折叠求值 + 上下文传递 ──────────
        if crate::fsm::chain::is_chain_command(&input) {
            return self.query_chained_magic(&input, env);
        }

        // 链式上游回退:`#` 被删空后 buffer 只剩上游文本(含 `'`,非 `#`/`/`
        // 开头)→ 回拼音组合继续编辑上游。
        if input.contains('\'') && !input.starts_with('#') && !input.starts_with('/') {
            self.state = ComposeState::Pinyin;
            self.comp.sync_preedit();
            return self.query_pinyin(env);
        }

        // ── Stage 2:家族统一查询(ensure/predict 内聚 MagicFamily,S5)──
        let answer = env
            .magic()
            .query(&mut self.magic.active, self.ctx, &input, env);
        // ── Stage 3:答案落位面板 ──
        self.magic.predictions = answer.predictions;
        self.magic.hints = answer.hints;
        self.magic.selectable = answer.selectable;
        self.rebuild_magic_view()
    }

    /// 链式命令模式(`X'#cmd`):上游折叠求值 → 命令段匹配 → 按上下文声明
    /// 分流(替换 / 拼接)。候选最终形态(`magic_predictions`)在此构造完成,
    /// `select_magic` / `rebuild_magic_view` 无需感知链式。
    ///
    /// - 感知上下文的命令([`MagicMember::wants_context`] = Some):拿上游
    ///   候选页(`first_text()` = 高亮首选;空链 `X''#t` 语义即整页),预测
    ///   **替换**候选列表;
    /// - 不感知的命令:普通 `predict`,非交互预测与上游首选**拼接**;
    ///   interactive 预测(命令会话内部导航)不参与拼接,原样显示;
    /// - 命令段未完成(`#`、`#x` 前缀/未知):候选 = 上游预测(用户可提前
    ///   选上游结果)或补全提示。
    ///
    /// MVP 注:上游取其候选 top1(命令模式下不导航上游;改上游请回格)。
    /// 链式进入前的造词半成品(`committed_text`)不参与上游求值。
    fn query_chained_magic(&mut self, input: &str, env: &dyn StepEnv) -> ImeView {
        use crate::fsm::chain::{join_segments, split_segments, ChainSeg};

        let segs = split_segments(input);
        let Some((ChainSeg::Command(cmd), prefix)) = segs.split_last() else {
            return self.rebuild_magic_view(); // 防御:is_chain_command 已保证
        };
        let (cmd, prefix) = (cmd.clone(), prefix.to_vec());
        let upstream_buf = join_segments(&prefix);
        let upstream_cands = self.eval_upstream(&upstream_buf, env);
        let upstream_first = upstream_cands.first().cloned().unwrap_or_default();

        match env.magic().match_command(&cmd) {
            MagicMatch::Exact(LiveCommand { token, name }) => {
                self.ensure_command(name, Some(token), env);
                // 上下文与语法严格对应:普通链(X'#cmd)只传高亮首选;
                // 空链(X''#cmd)传上游整页(#concat 类成员消费)。
                let upstream = ChainContext {
                    items: chain_context_items(&upstream_buf, &upstream_cands),
                };
                let wants = self.magic.active
                    .as_ref()
                    .and_then(|m| m.wants_context());
                let preds = match self.magic.active.as_mut() {
                    Some(member) => match wants {
                        Some(_) => member.predict_with_context(self.ctx, &cmd, &upstream, env),
                        None => member
                            .predict(self.ctx, &cmd, env)
                            .into_iter()
                            .map(|p| {
                                if p.interactive {
                                    p
                                } else {
                                    p.chained_prefix(&upstream_first)
                                }
                            })
                            .collect(),
                    },
                    None => Vec::new(),
                };
                self.magic.predictions = preds;
                self.magic.hints.clear();
                self.magic.selectable = cmd == format!("#{name}");
            }
            // 片段命令(X'#/hello):片段展开 × 上游拼接(片段不感知上下文)。
            MagicMatch::Snippet => {
                self.ensure_command("", Some("__SNIPPET__"), env);
                self.magic.predictions = self.magic.active
                    .as_mut()
                    .map(|m| m.predict(self.ctx, &cmd, env))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| {
                        if p.interactive {
                            p
                        } else {
                            p.chained_prefix(&upstream_first)
                        }
                    })
                    .collect();
                self.magic.hints.clear();
                self.magic.selectable = false;
            }
            MagicMatch::Args(LiveCommand { token, name }) => {
                self.ensure_command(name, Some(token), env);
                // 参数输入态(#del/15):裸输入提交候选;提交时 force_fire 带
                // 上游上下文强触发。
                self.magic.predictions = vec![Prediction::submit(input.to_string())];
                self.magic.hints.clear();
                self.magic.selectable = false;
            }
            MagicMatch::Prefix(hints) => {
                self.clear_active_command();
                self.magic.predictions.clear();
                self.magic.hints = hints;
                self.magic.selectable = true;
            }
            MagicMatch::Unknown => {
                // 命令段未知(# / #zzz):显示上游预测 —— 用户可选中上游结果
                // 直接提交,或继续编辑命令段。
                self.clear_active_command();
                self.magic.predictions = upstream_cands
                    .iter()
                    .take(7)
                    .map(|t| Prediction::commit(t.clone()))
                    .collect();
                self.magic.hints.clear();
                self.magic.selectable = !self.magic.predictions.is_empty();
            }
        }
        self.rebuild_magic_view()
    }

    /// 上游链折叠求值 → 候选文本列表(top8)。递归左折叠:前缀求值 →
    /// `First` 上下文传给最后一段;文本段走统一打分(`'` 组合由拼音家族
    /// 处理,即 P0),命令段临时 spawn 求值(级联中间命令不保异步会话 —
    /// 会话态只有活动命令有)。
    fn eval_upstream(&self, upstream: &str, env: &dyn StepEnv) -> Vec<String> {
        use crate::fsm::chain::{join_segments, split_segments, ChainSeg};

        if upstream.is_empty() {
            return Vec::new();
        }
        let segs = split_segments(upstream);
        let Some((last, prefix)) = segs.split_last() else {
            return Vec::new();
        };
        let prefix_buf = join_segments(prefix);
        // 命令段的上游 = 前缀折叠整页;文本段不需要上游对象(直接拼接)。
        let upstream_page = match last {
            ChainSeg::Command(_) if prefix_buf.is_empty() => Vec::new(),
            ChainSeg::Command(_) => self.eval_upstream(&prefix_buf, env),
            ChainSeg::Text(_) => Vec::new(),
        };
        let upstream_first = upstream_page.first().cloned().unwrap_or_default();
        match last {
            // 尾空链(X''):透传前缀整页 —— 空链语义:下一命令的上下文
            // 不是首选,是整页候选(X''#concat)。
            ChainSeg::Text(t) if t.is_empty() => {
                if prefix_buf.is_empty() {
                    Vec::new()
                } else {
                    self.eval_upstream(&prefix_buf, env)
                }
            }
            ChainSeg::Text(t) => {
                let ranked = env.scorer().rank_detailed(t, &self.context);
                let texts: Vec<String> = ranked.into_iter().map(|c| c.text).take(8).collect();
                if upstream_first.is_empty() {
                    texts
                } else {
                    texts
                        .into_iter()
                        .map(|t| format!("{upstream_first}{t}"))
                        .collect()
                }
            }
            ChainSeg::Command(c) => {
                let ctx = (!upstream_page.is_empty())
                    .then(|| ChainContext { items: upstream_page.clone() });
                self.eval_command(c, ctx.as_ref(), env)
            }
        }
    }

    /// 命令段求值(级联中间命令):临时 spawn + 上下文分流,产出候选文本
    /// (interactive 项是命令会话导航,中间级联无意义,过滤)。
    fn eval_command(
        &self,
        cmd: &str,
        upstream: Option<&ChainContext>,
        env: &dyn StepEnv,
    ) -> Vec<String> {
        match env.magic().match_command(cmd) {
            MagicMatch::Exact(LiveCommand { token, .. }) => {
                let Some(mut m) = env.magic().spawn(token) else {
                    return Vec::new();
                };
                let wants = m.wants_context();
                let preds = match (upstream, wants) {
                    (Some(u), Some(_)) => m.predict_with_context(self.ctx, cmd, u, env),
                    (Some(u), None) => {
                        let up = u.first_text().to_string();
                        m.predict(self.ctx, cmd, env)
                            .into_iter()
                            .map(|p| {
                                if p.interactive {
                                    p
                                } else {
                                    p.chained_prefix(&up)
                                }
                            })
                            .collect()
                    }
                    _ => m.predict(self.ctx, cmd, env),
                };
                preds
                    .into_iter()
                    .filter(|p| !p.interactive)
                    .map(|p| p.commit_value().to_string())
                    .take(8)
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// 精确匹配时复用同名命令实例(保 req 异步态),否则新建。
    fn ensure_command(
        &mut self,
        name: &'static str,
        token: Option<&'static str>,
        env: &dyn StepEnv,
    ) {
        let keep = self.magic.active
            .as_ref()
            .map(|m| m.name() == name)
            .unwrap_or(false);
        if keep {
            return;
        }
        self.clear_active_command();
        if let Some(tok) = token {
            self.magic.active = env.magic().spawn(tok);
        }
    }

    pub(crate) fn clear_active_command(&mut self) {
        if let Some(mut m) = self.magic.active.take() {
            m.deactivate(self.ctx);
        }
    }

    /// 异步命令 tick(round12:stage2 门面)—— 引擎壳的渲染循环轮询
    /// 活跃魔法命令(`#asr` / `#req`):驱动成员 tick,无新预测时重拉
    /// predict,然后重建面板视图。成员 take/put、predictions 回填全是
    /// MagicSession 内部事务,壳内禁止直接操作。
    /// 返回 None = 非 Snippet 态或无活跃成员(壳无需同步 flags)。
    pub(crate) fn magic_tick(&mut self, disp: &dyn StepEnv) -> Option<ImeView> {
        if self.state != ComposeState::Snippet {
            return None;
        }
        // 成员被取走以便自由变更状态机,随后放回(成员可能自行退出)。
        let mut member = self.magic.active.take()?;
        let new_preds = member.tick(self.ctx, &self.comp.buffer.clone(), disp);
        // Live 成员的 tick 当前返回 None(由 listener 主动 refresh_ui 触发);
        // 但 frontend 拉 magic_tick 时仍要拿到最新候选 —— 重新调 predict 一次。
        let preds = new_preds
            .unwrap_or_else(|| member.predict(self.ctx, &self.comp.buffer.clone(), disp));
        self.magic.active = Some(member);
        self.magic.predictions = preds;
        Some(self.rebuild_magic_view())
    }

    /// 从 `magic_predictions` / `magic_hints` 重建候选列表 + preedit + 视图。
    /// 候选 = [预测…, 补全…, rollback];preedit = 首条预测(精确)否则输入。
    pub(crate) fn rebuild_magic_view(&mut self) -> ImeView {
        let mut cands: Vec<String> = Vec::new();
        for p in &self.magic.predictions {
            cands.push(p.text.clone());
        }
        for h in &self.magic.hints {
            cands.push(h.clone());
        }
        // 参数输入态的裸提交候选文本 == 缓冲,不重复追加 rollback。
        let is_submit = self.magic.predictions.first().map(|p| p.submit).unwrap_or(false);
        if !is_submit {
            cands.push(self.comp.buffer.clone()); // rollback — 最后一项
        }
        self.panel.items = cands;
        self.panel.fresh = true;
        self.panel.highlight = 0;
        self.panel.page = 0;
        self.panel.full_comp_count = self.panel.items.len();
        self.panel.partial = vec![false; self.panel.items.len()];
        if let Some(head) = self.magic.predictions.first() {
            // preedit 用选项独立的预览文本(默认=展示文本)—— 允许候选行展示
            // 精简结果、文本框给完整预览。
            self.comp.preedit = head.preedit_value().to_string();
        } else {
            self.comp.preedit = self.comp.buffer.clone();
        }
        self.comp.cursor = self.comp.preedit.len();
        self.make_view()
    }

    /// 活跃 #asr 会话探测(round12:stage2 门面)—— 返回 (是否存活, 调试串)。
    /// 前端 refresh_ui 同步查它来告诉 voice server"这次刷新会不会被主循环
    /// 接受"。会话状态判断内聚在 stage2,壳只消费结果。
    pub(crate) fn asr_probe(&self) -> (bool, String) {
        let member_name = self.magic.active.as_ref().map(|m| m.name().to_string());
        let alive = self.state == ComposeState::Snippet &&
            member_name.as_deref() == Some("asr");
        let state = if self.state == ComposeState::Snippet { "Snippet" } else { "other" };
        (alive, format!("state={state} member={}", member_name.unwrap_or_else(|| "-".into())))
    }

    /// 选中候选(index):补全改写 / 预测提交(交互 or 上屏)/ rollback 提交。
    pub fn select_magic(&mut self, index: usize, env: &dyn StepEnv) -> ImeView {
        let n_preds = self.magic.predictions.len();
        let n_hints = self.magic.hints.len();
        // 1. 精确匹配的预测选项。
        if index < n_preds {
            let pred = self.magic.predictions[index].clone();
            // 参数输入态的裸输入提交 → 用完整输入重新解析,忽略 `/…` 参数,
            // 前缀匹配命令并**强制触发**(predict 会用完整输入解析删除/请求)。
            if pred.submit {
                return self.force_fire(env);
            }
            if pred.interactive {
                // 交互式:传给命令 → 重新预测,替换选项(不上屏)。
                if let Some(mut m) = self.magic.active.take() {
                    m.pick(index, &pred.text, self.ctx, env);
                    self.magic.active = Some(m);
                }
                return self.query_magic(env);
            }
            self.clear_active_command();
            self.reset();
            // `#del` 等删除选项:不提交文本,只让前端删 N 个字符。
            if pred.delete_count > 0 {
                return delete_view(pred.delete_count);
            }
            // 提交用 commit_text(展示转义时原文提交),光标针对展示文本。
            let commit = pred.commit_value().to_string();
            self.commit_text(&commit, None);
            return match pred.cursor {
                Some(c) => commit_view_at(&commit, c),
                None => commit_view(&commit),
            };
        }
        // 2. 补全提示:改写输入(不提交)。
        if index < n_preds + n_hints {
            let hint = self.magic.hints[index - n_preds].clone();
            self.comp.buffer = hint;
            self.magic.hints.clear();
            return self.query_magic(env);
        }
        // 3. rollback:提交原始缓冲。
        let raw = std::mem::take(&mut self.comp.buffer);
        self.commit_raw_and_reset(&raw)
    }

    /// 参数输入态的**裸输入提交**(`#del/15` + Space):用完整输入重新调用成员
    /// `predict` —— 成员解析参数后决定动作(删除 / 提交 / 交互请求)。取首条
    /// 预测执行;无预测则提交原始缓冲。
    fn force_fire(&mut self, env: &dyn StepEnv) -> ImeView {
        use crate::fsm::chain::{join_segments, split_segments, ChainSeg};

        let input = self.comp.buffer.clone();

        // 链式参数态(X'#del/15):命令段(含参数)提取,上游求值后带上下文
        // 强触发;不感知的命令照旧拼接。
        let preds = if crate::fsm::chain::is_chain_command(&input) {
            let segs = split_segments(&input);
            let (cmd, prefix) = match segs.split_last() {
                Some((ChainSeg::Command(c), p)) => (c.clone(), p.to_vec()),
                _ => (input.clone(), vec![]),
            };
            let upstream_buf = join_segments(&prefix);
            let upstream = ChainContext {
                items: chain_context_items(
                    &upstream_buf,
                    &self.eval_upstream(&upstream_buf, env),
                ),
            };
            match self.magic.active.as_mut() {
                Some(m) => {
                    if m.wants_context().is_some() {
                        m.predict_with_context(self.ctx, &cmd, &upstream, env)
                    } else {
                        let up = upstream.first_text().to_string();
                        m.predict(self.ctx, &cmd, env)
                            .into_iter()
                            .map(|p| {
                                if p.interactive {
                                    p
                                } else {
                                    p.chained_prefix(&up)
                                }
                            })
                            .collect()
                    }
                }
                None => Vec::new(),
            }
        } else {
            self.magic.active
                .as_mut()
                .map(|m| m.predict(self.ctx, &input, env))
                .unwrap_or_default()
        };
        if let Some(head) = preds.first().cloned() {
            if head.interactive {
                // 交互(如 addon 请求中…):展示为候选,等待异步落地。
                self.magic.predictions = preds;
                self.magic.hints.clear();
                self.magic.selectable = false;
                return self.rebuild_magic_view();
            }
            self.clear_active_command();
            self.reset();
            if head.delete_count > 0 {
                return delete_view(head.delete_count);
            }
            let commit = head.commit_value().to_string();
            self.commit_text(&commit, None);
            return match head.cursor {
                Some(c) => commit_view_at(&commit, c),
                None => commit_view(&commit),
            };
        }
        // 无预测 → 提交原始输入。
        let raw = std::mem::take(&mut self.comp.buffer);
        self.commit_raw_and_reset(&raw)
    }

    /// 魔法预测模式下,preedit(应用高亮"将提交")跟随候选高亮:
    /// 高亮在预测上 → 显示该预测;高亮在 rollback/补全上 → 显示原始输入。
    /// 拼音态不适用(拼音 preedit 是组合,不是候选)。
    pub(crate) fn sync_magic_preedit(&mut self) {
        if self.state != ComposeState::Snippet || self.magic.predictions.is_empty() {
            return;
        }
        let hl = self.panel.highlight;
        if let Some(p) = self.magic.predictions.get(hl) {
            self.comp.preedit = p.text.clone();
        } else {
            self.comp.preedit = self.comp.buffer.clone();
        }
        self.comp.cursor = self.comp.preedit.len();
    }
}

/// 链式上下文的裁剪:空链(`X''#cmd`,上游串以 `'` 结尾)→ 整页;普通链
/// (`X'#cmd`)→ 仅高亮首选。与语法语义严格一致(#concat 单链只拼首选)。
fn chain_context_items(upstream_buf: &str, cands: &[String]) -> Vec<String> {
    if upstream_buf.ends_with('\'') {
        cands.to_vec()
    } else {
        cands.first().cloned().into_iter().collect()
    }
}
