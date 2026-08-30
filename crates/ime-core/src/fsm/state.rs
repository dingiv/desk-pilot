//! IME composition state machine.
//!
//! ## State Transition Table
//!
//! **输入路由(哪个键进到这里、修饰键策略、透传判定)由
//! [`crate::router`](crate::router) 的状态机表统一决定** —— 本模块只描述
//! 字符进入组合后的内部迁移:
//!
//! | Current  | Input      | → Next   | View filled                |
//! |----------|------------|----------|----------------------------|
//! | Idle     | `/` `#`    | Snippet  | preedit_text               |
//! | Idle     | a-z        | Pinyin   | candidates or preedit_text  |
//! | Idle     | other      | Idle     | action=PASSTHROUGH        |
//! | Snippet  | letter/dig | Snippet  | trie step → commit/preedit |
//! | Snippet  | dead-end   | Idle     | commit_text                |
//! | Pinyin   | a-z        | Pinyin   | extend + fill_view         |
//! | Pinyin   | Space      | Idle     | commit_text                |
//! | Pinyin   | Enter      | Idle     | commit_text                |
//! | Pinyin   | Backspace  | P/Idle   | pop + fill_view            |
//! | Pinyin   | other      | Idle     | commit_text                |
//!
//! ## Incremental composition (造词)
//!
//! When the buffer contains 2+ syllables, the candidate list shows BOTH:
//!  - Full Viterbi compositions (select → commit entire word)
//!  - First-syllable single characters (select → commit that char, reduce buffer)
//!
//! After each partial commit the buffer shrinks and the query repeats.
//! When the last syllable is committed, the resulting phrase is saved to
//! the PhraseBook for future sessions.

use crate::expander::Expander;
use crate::family::magic::{
    ChainContext, MagicCommand, MagicMatch, MagicMember, Prediction,
};
use crate::matcher::Matcher;
use crate::frontend::{ImeView, CANDIDATE_SLOTS};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposeState {
    #[default]
    Idle,
    Snippet,
    Pinyin,
}

#[derive(Default)]
pub struct StateMachine {
    /// 所属输入上下文(引擎 `with_ctx` 每次操作前设置)。魔法命令成员用它
    /// 把异步工作事件发到正确的 ctx 并 refresh 对应上下文。
    pub ctx: usize,
    pub state: ComposeState,
    /// 键入的原始文本(保留大小写)。预测用 [`buffer`](小写);展示与提交
    /// 用这里。英文候选提交时按它回填大小写(English 而非 english)。
    /// 不变式:`buffer` 是 `raw_buffer` 的 ASCII 小写,二者等长。
    pub raw_buffer: String,
    /// Raw pinyin buffer — remaining uncommitted pinyin syllables.
    pub buffer: String,

    /// Visual preedit: committed hanzi + remaining pinyin.
    pub preedit: String,

    /// 调试模式:候选词显示提供者与权重(`[score family/source]`)。
    pub candidate_meta_enabled: bool,

    /// Cursor 管理
    /// Cursor byte offset within preedit.
    pub cursor: usize,

    /// 候选项管理
    pub candidates: Vec<String>,
    pub candidates_fresh: bool,
    pub candidate_highlight: usize,
    pub candidate_page: usize,
    pub candidate_page_size: usize,

    /// **pinyin 家族**
    /// 自生词系统
    /// Hanzi already committed during incremental composition (e.g. "李正").
    pub committed_text: String,
    /// Pinyin corresponding to the committed hanzi (e.g. "lizheng").
    committed_pinyin_buf: String,
    /// How many of the first candidates are full-word compositions
    /// (for backward compat and UI display offset).
    pub full_comp_count: usize,
    /// postprocess → query_pinyin 的带出槽(stage3 内部中间值,非持久状态)。
    pending_full_comp_count: usize,
    /// Indices in `candidates` that are single-char partial-commit options.
    partial_commit_indices: Vec<bool>,
    /// Short-term input context — accumulates recently committed text.
    pub context: crate::family::InputContext,
    /// 补全提示(输入是某命令触发串的严格前缀):候选 = [补全名…, rollback]。
    /// 选中补全名 → **改写输入**(不提交)。

    /// 魔法命令家族
    pub(crate) magic_hints: Vec<String>,
    /// 精确匹配命令时的预测选项(不含 rollback)。
    pub(crate) magic_predictions: Vec<crate::family::magic::Prediction>,
    /// 当前精确匹配的 live 命令实例(保 req 异步态等);静态命令 / 前缀 / 未知
    /// 时为 None。
    pub active_command: Option<Box<dyn MagicMember>>,
    /// 数字键是否用于选中候选(精确无参 / 前缀时 true;拼参数时 false)。
    magic_selectable: bool,

    /// 最近一次排名的详细结果(score, family, source)——与 candidates 对齐,
    /// 供 fill_view 填充 meta。
    last_meta: Vec<CandMeta>,

    /// 最近一次提交的候选家族(select 时从候选元数据取)。引擎提交点据此
    /// 判断:提交的是英文候选(来源 english)则不再学成自生词。
    /// FIXME: 需要移除
    pub(crate) last_commit_family: Option<&'static str>,
}

/// 一个候选词 + 它的元数据(来源家族/成员 + 权重)。调试 meta 与
/// "提交来源判断"(英文候选不再学成自生词)共用。
#[derive(Debug, Clone)]
pub(crate) struct CandMeta {
    pub text: String,
    pub score: f64,
    pub family: &'static str,
    pub source: &'static str,
}

/// Stage 3 后处理的产出项:候选文本 + 元数据 + 部分提交标记,三者同源。
/// candidates / last_meta / partial_commit_indices 三个列表由此单点派生
/// (S2 规范化:消除重排后独立数组间的对齐假设)。
#[derive(Debug, Clone)]
pub(crate) struct PanelItem {
    pub text: String,
    pub meta: CandMeta,
    pub partial: bool,
}

impl StateMachine {
    /// 最近一次排名的候选元数据(含文本,engine.view 的调试视图用)。
    pub(crate) fn last_meta(&self) -> &[CandMeta] {
        &self.last_meta
    }

    /// 取走并清空最近一次提交的候选家族(one-shot,引擎提交点读后置空,
    /// 避免残留在后续 raw 提交上)。
    pub(crate) fn take_last_commit_family(&mut self) -> Option<&'static str> {
        self.last_commit_family.take()
    }

    /// 面板镜像(S3 统一):last_meta → RankedCandidate,与 candidates
    /// 同序同源 —— 用户看见什么,这里就是什么。
    pub(crate) fn detailed(&self) -> Vec<crate::family::RankedCandidate> {
        self.last_meta
            .iter()
            .map(|m| crate::family::RankedCandidate {
                text: m.text.clone(),
                score: m.score,
                family: m.family,
                source: m.source,
            })
            .collect()
    }
}

impl std::fmt::Debug for StateMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateMachine")
            .field("state", &self.state)
            .field("buffer", &self.buffer)
            .field("preedit", &self.preedit)
            .field("cursor", &self.cursor)
            .field("candidates", &self.candidates)
            .field("candidate_highlight", &self.candidate_highlight)
            .field(
                "active_command",
                &self.active_command.as_ref().map(|m| m.name()),
            )
            .finish()
    }
}

impl StateMachine {
    pub fn new() -> Self {
        StateMachine::with_page_size(7)
    }

    /// Construct with a configurable candidate page size (default 7).
    /// The engine passes `swift-ime.yaml → input.page_size` via
    /// [`ImeEngine::set_page_size`](crate::engine::ImeEngine::set_page_size).
    pub fn with_page_size(page_size: u32) -> Self {
        StateMachine {
            candidate_page_size: page_size.max(1) as usize,
            ..StateMachine::default()
        }
    }

    pub fn step(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        match self.state {
            ComposeState::Idle => self.handle_idle(ch, env),
            ComposeState::Snippet => self.handle_snippet(ch, env),
            ComposeState::Pinyin => self.handle_pinyin(ch, env),
        }
    }

    // ── Magic command prediction (Snippet state) ────────────────────────

    /// `#asr?num=2` → `#asr`(名字段,用于静态命令展开 / 无参判定)。
    fn command_trigger(input: &str) -> String {
        if input.len() < 2 {
            return input.to_string();
        }
        let rest = &input[1..];
        let name_len = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        format!("#{}", &rest[..name_len])
    }

    /// 每次字符变化后重查:精确匹配 → 命令预测;前缀 → 补全提示;未知 → raw。
    fn query_magic(&mut self, env: &dyn StepEnv) -> ImeView {
        let input = self.buffer.clone();

        // ── 链式命令模式(X'#cmd):上游折叠求值 + 上下文传递 ──────────
        if crate::fsm::chain::is_chain_command(&input) {
            return self.query_chained_magic(&input, env);
        }

        // 链式上游回退:`#` 被删空后 buffer 只剩上游文本(含 `'`,非 `#`/`/`
        // 开头)→ 回拼音组合继续编辑上游。
        if input.contains('\'') && !input.starts_with('#') && !input.starts_with('/') {
            self.state = ComposeState::Pinyin;
            self.preedit = format!("{}{}", self.committed_text, self.raw_buffer);
            self.cursor = self.preedit.len();
            return self.query_pinyin(env);
        }

        // 分隔符不参与命令匹配:`'` 是链结构字符(chain.rs),追加它不改变命令
        // 语义 —— `#asr'` 的候选保持 `#asr` 的预测结果不变;用户继续输入
        // (`#asr'#tr`)时由上面的 is_chain_command 分支接管。剥掉尾部全部 `'`
        // (含 `#asr''` 空链准备态);命令文本/提交候选同样不含 `'`。
        let match_input = input.trim_end_matches('\'').to_string();
        match env.magic().match_command(&match_input) {
            // TODO: 不用区分了, 魔法命令全都是 LIVE
            MagicMatch::Exact(cmd) => match cmd {
                MagicCommand::Live { token, name } => {
                    self.ensure_command(name, Some(token), env);
                    self.magic_predictions = self
                        .active_command
                        .as_mut()
                        // TODO: magic 命令调用点
                        .map(|m| m.predict(self.ctx, &match_input, env))
                        .unwrap_or_default();
                    self.magic_hints.clear();
                    // 无参数时数字用于选中;有参数(拼 `?num=` 等)时数字是文本。
                    self.magic_selectable = match_input == format!("#{name}");
                }
                MagicCommand::Static => {
                    self.clear_active_command();
                    let trigger = Self::command_trigger(&match_input);
                    self.magic_predictions =
                        env.magic().static_prediction(&trigger).unwrap_or_default();
                    self.magic_hints.clear();
                    self.magic_selectable = match_input == trigger;
                }
            },
            // 参数输入态(`#del/1`):不调 member.predict(不自动触发),展示裸输入
            // 提交候选;数字是参数文本。提交时 force-fire 解析前缀再触发。
            // (匹配逻辑只对 live 成员产生 Args,静态命令不会带参数。)
            MagicMatch::Args(MagicCommand::Live { token, name }) => {
                self.ensure_command(name, Some(token), env);
                self.magic_predictions = vec![Prediction::submit(match_input.clone())];
                self.magic_hints.clear();
                self.magic_selectable = false;
            }
            MagicMatch::Args(MagicCommand::Static) => {
                self.clear_active_command();
                self.magic_predictions.clear();
                self.magic_hints.clear();
                self.magic_selectable = false;
            }
            MagicMatch::Prefix(hints) => {
                self.clear_active_command();
                self.magic_predictions.clear();
                self.magic_hints = hints;
                self.magic_selectable = true; // 前缀 → 数字选中补全
            }
            MagicMatch::Snippet => {
                self.ensure_command("", Some("__SNIPPET__"), env);
                self.magic_predictions = self
                    .active_command
                    .as_mut()
                    .map(|m| m.predict(self.ctx, &match_input, env))
                    .unwrap_or_default();
                self.magic_hints.clear();
                self.magic_selectable = false; // 片段路径/查询里的数字是文本
            }
            MagicMatch::Unknown => {
                self.clear_active_command();
                self.magic_predictions.clear();
                self.magic_hints.clear();
                self.magic_selectable = false;
            }
        }
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
            MagicMatch::Exact(MagicCommand::Live { token, name }) => {
                self.ensure_command(name, Some(token), env);
                // 上下文与语法严格对应:普通链(X'#cmd)只传高亮首选;
                // 空链(X''#cmd)传上游整页(#concat 类成员消费)。
                let upstream = ChainContext {
                    items: chain_context_items(&upstream_buf, &upstream_cands),
                };
                let wants = self
                    .active_command
                    .as_ref()
                    .and_then(|m| m.wants_context());
                let preds = match self.active_command.as_mut() {
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
                self.magic_predictions = preds;
                self.magic_hints.clear();
                self.magic_selectable = cmd == format!("#{name}");
            }
            MagicMatch::Exact(MagicCommand::Static) => {
                self.clear_active_command();
                let trigger = Self::command_trigger(&cmd);
                self.magic_predictions = env
                    .magic()
                    .static_prediction(&trigger)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| p.chained_prefix(&upstream_first))
                    .collect();
                self.magic_hints.clear();
                self.magic_selectable = cmd == trigger;
            }
            // 片段命令(X'#/hello):片段展开 × 上游拼接(片段不感知上下文)。
            MagicMatch::Snippet => {
                self.ensure_command("", Some("__SNIPPET__"), env);
                self.magic_predictions = self
                    .active_command
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
                self.magic_hints.clear();
                self.magic_selectable = false;
            }
            MagicMatch::Args(MagicCommand::Live { token, name }) => {
                self.ensure_command(name, Some(token), env);
                // 参数输入态(#del/15):裸输入提交候选;提交时 force_fire 带
                // 上游上下文强触发。
                self.magic_predictions = vec![Prediction::submit(input.to_string())];
                self.magic_hints.clear();
                self.magic_selectable = false;
            }
            MagicMatch::Args(MagicCommand::Static) => {
                self.clear_active_command();
                self.magic_predictions.clear();
                self.magic_hints.clear();
                self.magic_selectable = false;
            }
            MagicMatch::Prefix(hints) => {
                self.clear_active_command();
                self.magic_predictions.clear();
                self.magic_hints = hints;
                self.magic_selectable = true;
            }
            MagicMatch::Unknown => {
                // 命令段未知(# / #zzz):显示上游预测 —— 用户可选中上游结果
                // 直接提交,或继续编辑命令段。
                self.clear_active_command();
                self.magic_predictions = upstream_cands
                    .iter()
                    .take(7)
                    .map(|t| Prediction::commit(t.clone()))
                    .collect();
                self.magic_hints.clear();
                self.magic_selectable = !self.magic_predictions.is_empty();
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
            MagicMatch::Exact(MagicCommand::Live { token, .. }) => {
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
            MagicMatch::Exact(MagicCommand::Static) => {
                let trigger = Self::command_trigger(cmd);
                env.magic()
                    .static_prediction(&trigger)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| match upstream {
                        Some(u) => p.chained_prefix(u.first_text()),
                        None => p,
                    })
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
        let keep = self
            .active_command
            .as_ref()
            .map(|m| m.name() == name)
            .unwrap_or(false);
        if keep {
            return;
        }
        self.clear_active_command();
        if let Some(tok) = token {
            self.active_command = env.magic().spawn(tok);
        }
    }

    fn clear_active_command(&mut self) {
        if let Some(mut m) = self.active_command.take() {
            m.deactivate(self.ctx);
        }
    }

    /// 从 `magic_predictions` / `magic_hints` 重建候选列表 + preedit + 视图。
    /// 候选 = [预测…, 补全…, rollback];preedit = 首条预测(精确)否则输入。
    pub(crate) fn rebuild_magic_view(&mut self) -> ImeView {
        let mut cands: Vec<String> = Vec::new();
        for p in &self.magic_predictions {
            cands.push(p.text.clone());
        }
        for h in &self.magic_hints {
            cands.push(h.clone());
        }
        // 参数输入态的裸提交候选文本 == 缓冲,不重复追加 rollback。
        let is_submit = self.magic_predictions.first().map(|p| p.submit).unwrap_or(false);
        if !is_submit {
            cands.push(self.buffer.clone()); // rollback — 最后一项
        }
        self.candidates = cands;
        self.candidates_fresh = true;
        self.candidate_highlight = 0;
        self.candidate_page = 0;
        self.full_comp_count = self.candidates.len();
        self.partial_commit_indices = vec![false; self.candidates.len()];
        if let Some(head) = self.magic_predictions.first() {
            // preedit 用选项独立的预览文本(默认=展示文本)—— 允许候选行展示
            // 精简结果、文本框给完整预览。
            self.preedit = head.preedit_value().to_string();
        } else {
            self.preedit = self.buffer.clone();
        }
        self.cursor = self.preedit.len();
        self.make_view()
    }

    /// 选中候选(index):补全改写 / 预测提交(交互 or 上屏)/ rollback 提交。
    pub fn select_magic(&mut self, index: usize, env: &dyn StepEnv) -> ImeView {
        let n_preds = self.magic_predictions.len();
        let n_hints = self.magic_hints.len();
        // 1. 精确匹配的预测选项。
        if index < n_preds {
            let pred = self.magic_predictions[index].clone();
            // 参数输入态的裸输入提交 → 用完整输入重新解析,忽略 `/…` 参数,
            // 前缀匹配命令并**强制触发**(predict 会用完整输入解析删除/请求)。
            if pred.submit {
                return self.force_fire(env);
            }
            if pred.interactive {
                // 交互式:传给命令 → 重新预测,替换选项(不上屏)。
                if let Some(mut m) = self.active_command.take() {
                    m.pick(index, &pred.text, self, env);
                    self.active_command = Some(m);
                }
                return self.query_magic(env);
            }
            self.clear_active_command();
            self.reset();
            // `#del` 等删除选项:不提交文本,只让前端删 N 个字符。
            if pred.delete_count > 0 {
                return Self::delete_view(pred.delete_count);
            }
            // 提交用 commit_text(展示转义时原文提交),光标针对展示文本。
            let commit = pred.commit_value().to_string();
            return match pred.cursor {
                Some(c) => Self::commit_view_at(&commit, c),
                None => Self::commit_view(&commit),
            };
        }
        // 2. 补全提示:改写输入(不提交)。
        if index < n_preds + n_hints {
            let hint = self.magic_hints[index - n_preds].clone();
            self.buffer = hint;
            self.magic_hints.clear();
            return self.query_magic(env);
        }
        // 3. rollback:提交原始缓冲。
        let raw = std::mem::take(&mut self.buffer);
        self.reset();
        Self::commit_view(&raw)
    }

    /// 参数输入态的**裸输入提交**(`#del/15` + Space):用完整输入重新调用成员
    /// `predict` —— 成员解析参数后决定动作(删除 / 提交 / 交互请求)。取首条
    /// 预测执行;无预测则提交原始缓冲。
    fn force_fire(&mut self, env: &dyn StepEnv) -> ImeView {
        use crate::fsm::chain::{join_segments, split_segments, ChainSeg};

        let input = self.buffer.clone();

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
            match self.active_command.as_mut() {
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
            self.active_command
                .as_mut()
                .map(|m| m.predict(self.ctx, &input, env))
                .unwrap_or_default()
        };
        if let Some(head) = preds.first().cloned() {
            if head.interactive {
                // 交互(如 addon 请求中…):展示为候选,等待异步落地。
                self.magic_predictions = preds;
                self.magic_hints.clear();
                self.magic_selectable = false;
                return self.rebuild_magic_view();
            }
            self.clear_active_command();
            self.reset();
            if head.delete_count > 0 {
                return Self::delete_view(head.delete_count);
            }
            let commit = head.commit_value().to_string();
            return match head.cursor {
                Some(c) => Self::commit_view_at(&commit, c),
                None => Self::commit_view(&commit),
            };
        }
        // 无预测 → 提交原始输入。
        let raw = std::mem::take(&mut self.buffer);
        self.reset();
        Self::commit_view(&raw)
    }

    /// Select candidate at `index`.
    ///
    /// Full commit (`index < full_comp_count`): commits everything, records
    /// the pick in inputx-pinyin's L0 user model for frequency boosting.
    /// Multi-step compositions also save to the PhraseBook for recall.
    ///
    /// Partial commit (`index >= full_comp_count`): appends the single
    /// character to [`committed_text`], shrinks the buffer by one syllable,
    /// and re-queries. The character pick is also recorded in L0.
    pub fn select(&mut self, index: usize, env: &dyn StepEnv) -> ImeView {
        let picked = self.candidates.get(index).cloned().unwrap_or_default();
        if picked.is_empty() {
            return self.make_view();
        }

        let is_partial = self
            .partial_commit_indices
            .get(index)
            .copied()
            .unwrap_or(false);
        if !is_partial {
            // Full commit: combine committed_text + selected text. 英文候选按
            // 键入的原始大小写回填(raw_buffer),汉字候选天然 no-op。
            let picked_cased = apply_input_casing(&picked, &self.raw_buffer);
            let final_text = if self.committed_text.is_empty() {
                picked_cased.clone()
            } else {
                format!("{}{}", self.committed_text, picked_cased)
            };
            let full_pinyin = if self.committed_text.is_empty() {
                self.buffer.clone()
            } else {
                format!("{}{}", self.committed_pinyin(), self.buffer)
            };
            // 提交候选的来源家族 —— 两个家族的单词本各自闭环:
            // 拼音提交 → 拼音 L0/单词本;英文提交 → 英文家族(且词典词不学)。
            let commit_family = self
                .last_meta
                .iter()
                .find(|m| m.text == picked)
                .map(|m| m.family);
            // L0 频率加成只对拼音族提交生效 —— 英文候选提交不写拼音模型。
            if commit_family == Some("pinyin") {
                env.record_pick(&full_pinyin, &final_text);
            }
            // 自生词模式(拼音族):唯一的学习入口。经历过 ≥1 次数字键逐字选择
            // (committed_text 非空)后提交,整体无条件加入单词本。
            // 直接提交(空格选 top,未逐字选择)**不学** —— decomp 选项
            // 下次输入时 Viterbi 会重新组合出同样的候选,无需入本。
            if !self.committed_text.is_empty() {
                env.learn_composed_phrase(&full_pinyin, &final_text);
            }
            self.context.update(&final_text);
            // 记录提交候选的来源家族(供引擎判断是否学成自生词 —— 英文候选不学)。
            self.reset();
            self.last_commit_family = commit_family;
            Self::commit_view(&final_text)
        } else {
            // Partial commit: append this single character, shrink buffer.
            self.committed_text.push_str(&picked);
            let first_syl = env.first_syllable(&self.buffer).unwrap_or_default();
            let first_len = first_syl.len();
            if first_len > 0 && first_len <= self.buffer.len() {
                // Record this single-char pick in L0.
                let consumed = self.buffer[..first_len].to_string();
                env.record_pick(&consumed, &picked);
                self.committed_pinyin_buf.push_str(&consumed);
                self.buffer = self.buffer[first_len..].to_string();
                // 同步收缩 raw_buffer(consumed 是小写音节,等字节长)。
                self.raw_buffer = self.raw_buffer[first_len..].to_string();
            }
            self.preedit = format!("{}{}", self.committed_text, self.raw_buffer);
            self.cursor = self.preedit.len();
            self.candidates_fresh = false;
            self.candidate_highlight = 0;
            self.query_pinyin(env)
        }
    }

    pub fn reset(&mut self) {
        self.clear_active_command();
        self.state = ComposeState::Idle;
        self.buffer.clear();
        self.raw_buffer.clear();
        self.preedit.clear();
        self.cursor = 0;
        self.candidates.clear();
        self.candidate_highlight = 0;
        self.candidate_page = 0;
        self.candidates_fresh = false;
        self.committed_text.clear();
        self.committed_pinyin_buf.clear();
        self.full_comp_count = 0;
        self.partial_commit_indices.clear();
        self.magic_hints.clear();
        self.magic_predictions.clear();
        self.magic_selectable = false;
        // last_commit_family 在 select 之后(重置之后)设置,由引擎取走;此处
        // 清掉以防残留。select 内部会先 reset 再设,不受影响。
        self.last_commit_family = None;
    }

    /// Is the candidate panel OPEN (non-empty candidate list)? Navigation/paging special keys
    /// only act while it's open; when closed they pass through to the application.
    pub fn candidate_panel_open(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub fn move_highlight(&mut self, delta: i32) {
        if self.candidates.is_empty() {
            return;
        }
        let new = (self.candidate_highlight as i32 + delta)
            .clamp(0, self.candidates.len() as i32 - 1) as usize;
        self.candidate_highlight = new;
        if self.candidate_page_size > 0 {
            self.candidate_page = (new as u32)
                .checked_div(self.candidate_page_size as u32)
                .unwrap_or(0) as usize;
        }
        // 魔法命令预测:应用高亮(将提交)跟随高亮移动。
        self.sync_magic_preedit();
    }

    /// 魔法预测模式下,preedit(应用高亮"将提交")跟随候选高亮:
    /// 高亮在预测上 → 显示该预测;高亮在 rollback/补全上 → 显示原始输入。
    /// 拼音态不适用(拼音 preedit 是组合,不是候选)。
    pub(crate) fn sync_magic_preedit(&mut self) {
        if self.state != ComposeState::Snippet || self.magic_predictions.is_empty() {
            return;
        }
        let hl = self.candidate_highlight;
        if let Some(p) = self.magic_predictions.get(hl) {
            self.preedit = p.text.clone();
        } else {
            self.preedit = self.buffer.clone();
        }
        self.cursor = self.preedit.len();
    }

    /// Full pinyin for the committed portion.
    fn committed_pinyin(&self) -> String {
        self.committed_pinyin_buf.clone()
    }

    /// 把 preedit 里的字面换行转义成 `\n` 文本(展示用),并同步调整光标字节偏移:
    /// 每个**光标之前**的 `\n`(1 字节)→ `\n` 两字符(2 字节),光标后移 1 字节。
    /// 提交/拼写等不含 `\n` 的场景原样返回,零开销。
    fn escape_preedit(text: &str, cursor: usize) -> (String, usize) {
        if !text.contains('\n') {
            return (text.to_string(), cursor);
        }
        let mut escaped = String::with_capacity(text.len() + 4);
        let mut out_cursor = cursor;
        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                escaped.push_str("\\n");
                if i < cursor {
                    out_cursor += 1; // 光标前的 `\n` 扩成两字节,光标后移 1
                }
            } else {
                escaped.push(ch);
            }
        }
        (escaped, out_cursor)
    }

    // ── view helpers ────────────────────────────────────────────────────

    fn fill_view(&self, view: &mut ImeView) {
        // preedit 是应用文本框里显示的内容;多行片段(如 #/angle 三角形)含
        // 字面 `\n`,直接显示会破坏应用排版 → 转义成 `\n` 文本。光标字节偏移
        // 同步调整(每个光标前的 `\n` 多占 1 字节)。
        let (escaped, display_cursor) = Self::escape_preedit(&self.preedit, self.cursor);
        ImeView::set_str(&mut view.preedit_text, &escaped);
        view.preedit_cursor = display_cursor as u32;
        // ── 翻页窗口:16 槽 = 从当前页首起的滑动窗口 ──
        // view.candidates 定长(协议),但内容跟随 candidate_page 滑动 ——
        // merged 全量候选(如造词单字区 15+ 个、全量链)翻页全部可达。
        // view.candidate_highlight 同步换算成窗口内序(addon 直接用作列表
        // 光标);candidate_page 仍是页号(addon 据此算选词的全局序)。
        let page_size = self.candidate_page_size.max(1);
        let start = (self.candidate_page * page_size).min(self.candidates.len());
        let window = &self.candidates[start.min(self.candidates.len())..];
        let n = window.len().min(CANDIDATE_SLOTS);
        for i in 0..n {
            ImeView::set_str(&mut view.candidates[i].text, &window[i]);
            // Mark single-char partial-commit candidates with ">" label.
            if self.partial_commit_indices.get(start + i).copied().unwrap_or(false) {
                ImeView::set_str(&mut view.candidates[i].label, ">");
            }
            // 调试模式:候选词后附提供者与权重。
            if self.candidate_meta_enabled {
                if let Some(m) = self.last_meta.get(start + i) {
                    let meta = format!("[{:.3} {}/{}]", m.score, m.family, m.source);
                    ImeView::set_str(&mut view.candidates[i].meta, &meta);
                }
            }
        }
        view.candidate_count = n as u32;
        view.candidate_highlight = self.candidate_highlight.saturating_sub(start) as u32;
        view.candidate_page = self.candidate_page as u32;
        view.candidate_page_size = self.candidate_page_size as u32;
        // aux_up(候选框顶部)= **原始输入**(你打了什么),与 preedit_text(应用
        // 高亮,将提交的合成结果)严格区分。命令态显示 `#asr`,拼音态显示
        // 正在打的拼音(raw_buffer,保留大小写)。
        let raw = match self.state {
            ComposeState::Snippet => self.buffer.clone(),
            ComposeState::Pinyin => self.raw_buffer.clone(),
            ComposeState::Idle => String::new(),
        };
        ImeView::set_str(&mut view.aux_up, &raw);
    }

    /// Build a view from the current state (no key processed). Used by the state
    /// machine itself and by magic members rendering their candidates.
    pub(crate) fn make_view(&self) -> ImeView {
        let mut v = ImeView::empty();
        self.fill_view(&mut v);
        v.action = crate::frontend::action::HANDLED;
        v
    }

    pub(crate) fn commit_view(text: &str) -> ImeView {
        // Default: caret at the end of the committed text.
        let mut v = ImeView::empty();
        ImeView::set_str(&mut v.commit_text, text);
        v.commit_cursor = ImeView::str_field(&v.commit_text).len() as u32;
        v.action = crate::frontend::action::COMMIT | crate::frontend::action::HANDLED;
        v
    }

    /// Commit with the application caret placed at `cursor` (byte offset into the
    /// committed text) — snippet templates with `$CURSOR` land here. Clamped to
    /// the actually-committed length (the buffer may truncate long text).
    pub(crate) fn commit_view_at(text: &str, cursor: usize) -> ImeView {
        let mut v = ImeView::empty();
        ImeView::set_str(&mut v.commit_text, text);
        let len = ImeView::str_field(&v.commit_text).len();
        v.commit_cursor = cursor.min(len) as u32;
        v.action = crate::frontend::action::COMMIT | crate::frontend::action::HANDLED;
        v
    }

    /// View that passes the current key through to the application untouched.
    pub(crate) fn passthrough_view() -> ImeView {
        let mut v = ImeView::empty();
        v.action = crate::frontend::action::PASSTHROUGH;
        v
    }

    /// 删除视图:不提交文本,只让前端删掉文本框中 `count` 个字符(`#del`)。
    pub(crate) fn delete_view(count: u32) -> ImeView {
        tracing::debug!(count, "delete_view → ImeView.delete_count");
        let mut v = ImeView::empty();
        v.delete_count = count;
        v.action = crate::frontend::action::HANDLED;
        v
    }

    // ── Idle ───────────────────────────────────────────────────────────

    fn handle_idle(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        if env.matcher().is_trigger_prefix(ch) {
            self.state = ComposeState::Snippet;
            self.buffer.push(ch);
            self.preedit = self.buffer.clone();
            self.cursor = 1;
            return self.make_view();
        }
        if ch.is_ascii_alphabetic() {
            // 大写字母视作小写进行预测(English → english),展示与提交
            // 保留原始大小写(raw_buffer)。
            self.state = ComposeState::Pinyin;
            self.buffer.push(ch.to_ascii_lowercase());
            self.raw_buffer.push(ch);
            self.preedit = self.raw_buffer.clone();
            self.cursor = self.preedit.len();
            self.candidates_fresh = false;
            return self.query_pinyin(env);
        }
        Self::passthrough_view()
    }

    // ── Snippet ────────────────────────────────────────────────────────

    /// Snippet 态:所有 `#…` 输入统一在此处理。
    ///
    /// - Backspace 删字符重查;Enter 强选原始文本;
    /// - Space 选中高亮候选(预测提交 / 补全改写 / rollback 提交);
    /// - 数字键在可选中态(精确无参 / 前缀)选中候选,否则作为命令文本;
    /// - 其它字符追加后重查。
    fn handle_snippet(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        // Backspace: pop last char, re-query. Empty → reset.
        if ch == '\x08' {
            self.buffer.pop();
            if self.buffer.is_empty() {
                self.reset();
                return ImeView::empty();
            }
            return self.query_magic(env);
        }

        // Enter: force raw text.
        if ch == '\n' || ch == '\r' {
            let raw = std::mem::take(&mut self.buffer);
            self.reset();
            return Self::commit_view(&raw);
        }

        // Space: commit the highlighted candidate.
        if ch == ' ' {
            let hl = self
                .candidate_highlight
                .min(self.candidates.len().saturating_sub(1));
            return self.select_magic(hl, env);
        }

        // 数字键:可选中时选中候选,否则作为命令文本追加(如 `?num=2`)。
        if let d @ '1'..='9' = ch {
            if self.magic_selectable {
                let idx = (d as u8 - b'1') as usize;
                if idx < self.candidates.len() {
                    return self.select_magic(idx, env);
                }
            }
        }

        // 其它字符:追加到缓冲,重查。分字符键(`'`)附带"我说完了"信号 ——
        // 语音会话进行中时让 aura 立即归档开放窗口(整窗 batch,跳过
        // merge_gap 等待);无语音会话时 voice_cmd_tx 为 None,零开销跳过。
        if ch == '\'' {
            if let Some(tx) = env.voice_cmd_tx() {
                tx.send(crate::io_thread::VoiceCmd::FlushParagraph);
            }
        }
        self.buffer.push(ch);
        self.query_magic(env)
    }

    // ── Pinyin ─────────────────────────────────────────────────────────

    fn handle_pinyin(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        match ch {
            '\x08' => self.pinyin_backspace(env),
            '\n' | '\r' => self.pinyin_enter(),
            ' ' => self.pinyin_space(env),
            // 链分隔符:`'` 是组合内结构字符(ti'an 的两条链),不是终结符。
            // 追加进 buffer;预测层(拼音家族)按 `'` 切链组合。回格删 `'`
            // 天然回到无链状态 —— 链结构纯由 buffer 内容决定,无隐藏状态。
            // 附带"我说完了"信号:语音会话在听时让 aura 立即归档开放窗口
            // (整窗 batch,跳过 merge_gap 等待);无语音会话 → tx 为 None,跳过。
            '\'' => {
                if let Some(tx) = env.voice_cmd_tx() {
                    tx.send(crate::io_thread::VoiceCmd::FlushParagraph);
                }
                self.buffer.push('\'');
                self.raw_buffer.push('\'');
                self.preedit = format!("{}{}", self.committed_text, self.raw_buffer);
                self.cursor = self.preedit.len();
                self.candidates_fresh = false;
                self.query_pinyin(env)
            }
            // 链式命令:`'#` 序列开启命令链(X'#translate)。`#` 不终结组合、
            // 不提交 —— 上游链保留在 buffer 里,转入 Snippet 态做命令输入;
            // 单独的 `#`(无 `'` 前导)维持旧的终结符行为(提交候选 + `#`)。
            '#' if self.buffer.ends_with('\'') => {
                self.buffer.push('#');
                self.raw_buffer.push('#');
                self.state = ComposeState::Snippet;
                self.preedit = format!("{}{}", self.committed_text, self.raw_buffer);
                self.cursor = self.preedit.len();
                self.candidates_fresh = false;
                self.query_magic(env)
            }
            c if c.is_ascii_alphabetic() => {
                self.buffer.push(c.to_ascii_lowercase());
                self.raw_buffer.push(c);
                self.preedit = format!("{}{}", self.committed_text, self.raw_buffer);
                self.cursor = self.preedit.len();
                self.candidates_fresh = false;
                self.query_pinyin(env)
            }
            c => self.pinyin_terminator(c),
        }
    }

    fn query_pinyin(&mut self, env: &dyn StepEnv) -> ImeView {
        // ── Stage 2:家族预测(scorer 单点调用;合成段 S6 归 stage3)──
        let ranked = env.scorer().rank_detailed(&self.buffer, &self.context);
        // ── Stage 3:后处理统一管线 ──
        let items = self.postprocess(ranked, env);

        // 三列表同源同序落位 —— fill_view 的窗口偏移 / select 的家族判定 /
        // ">" 部分提交标记全从同一 PanelItem 序列出发,不再有独立数组间的
        // 对齐假设(修复:meta 采样曾在重排前,last_meta 与 candidates 错位)。
        self.full_comp_count = self.pending_full_comp_count;
        self.candidates = items.iter().map(|i| i.text.clone()).collect();
        self.partial_commit_indices = items.iter().map(|i| i.partial).collect();
        self.last_meta = items.iter().map(|i| i.meta.clone()).collect();

        let cands = self.candidates.clone();
        if !cands.is_empty() {
            self.candidate_highlight = 0;
            self.candidate_page = 0;
            self.candidates_fresh = true;
        } else {
            self.candidates.clear();
            self.candidates_fresh = false;
        }
        self.make_view()
    }

    /// Stage 3 后处理:全局调整(promote_single_letter)→ 造词单字区重排
    /// → 产出与 candidates 同序的 PanelItem 序列(meta/partial 同源)。
    /// full_comp_count 经 `pending_full_comp_count` 带出(postprocess 需
    /// &mut self 读 buffer/查询家族,而 query_pinyin 的落位段统一写)。
    fn postprocess(&mut self, ranked: Vec<crate::family::RankedCandidate>, env: &dyn StepEnv) -> Vec<PanelItem> {
        // 1. 全局调整:单字母输入的 self/case 置顶(与 candidates_detailed
        //    镜像同规则,见 engine.rs 同名调用)。
        let ranked = promote_single_letter(&self.buffer, ranked);
        // 2. PanelItem 化:meta 与文本同源。
        let mut items: Vec<PanelItem> = ranked
            .into_iter()
            .map(|c| PanelItem {
                meta: CandMeta {
                    text: c.text.clone(),
                    score: c.score,
                    family: c.family,
                    source: c.source,
                },
                text: c.text,
                partial: false,
            })
            .collect();

        // 3. Layer 3 造词单字区:内部逻辑(链式豁免/多音节判定/首音节/
        //    单字过滤)在拼音家族(compose_single_chars),壳只管调用与
        //    面板重排 —— 家族返回空(链式/单音节)即不重排,原序直出。
        {
            {
                // 词头只收**真词**(非 decomp 链:X食品 类拼接对逐字造词
                // 无价值);嵌入词典场景全 decomp 时保底收首候选(nihao 的
                // "你好"也是链,head 空会让单字区顶到槽 1,space 变部分提交)。
                let real_words: Vec<usize> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| it.meta.source != "decomp")
                    .map(|(i, _)| i)
                    .take(4)
                    .collect();
                let real_words = if real_words.is_empty() { vec![0] } else { real_words };
                let texts: Vec<String> = items.iter().map(|it| it.text.clone()).collect();
                // 单字区全量放出:merged 可超 16 槽,fill_view 按页切窗后
                // 翻页全部可达。
                let char_items: Vec<PanelItem> = env
                    .compose_single_chars(&self.buffer, &self.context, &texts, 32)
                    .into_iter()
                    .map(|c| PanelItem {
                        meta: CandMeta {
                            text: c.text.clone(),
                            // 家族内 raw 分(meta 显示语义与合成分类别不同,
                            // 单字区未过 ×priority —— 翻译标注见 weight-scoring.md)
                            score: c.raw_score,
                            family: c.family,
                            source: c.source,
                        },
                        text: c.text,
                        partial: true,
                    })
                    .collect();

                if !char_items.is_empty() {
                    let head: Vec<PanelItem> = real_words
                        .iter()
                        .filter_map(|&i| items.get(i).cloned())
                        .collect();
                    let head_len = head.len();
                    // tail:其余全部(head 之外的真词与链都保留 —— 旧实现
                    // skip(max_full) 会静默丢掉词头窗口里的链)。
                    let tail: Vec<PanelItem> = items
                        .into_iter()
                        .filter(|it| !head.iter().any(|h| h.text == it.text))
                        .collect();
                    let mut out = head;
                    out.extend(char_items);
                    out.extend(tail);
                    self.pending_full_comp_count = head_len;
                    return out;
                }
            }
        }
        self.pending_full_comp_count = items.len();
        items
    }

    fn pinyin_backspace(&mut self, env: &dyn StepEnv) -> ImeView {
        // If we have committed text, backspace undoes the last committed char.
        if !self.committed_text.is_empty() {
            self.committed_text.pop();
            // Undo the last consumed syllable from committed_pinyin_buf.
            let last_syl = env.first_syllable(&self.committed_pinyin_buf);
            if let Some(syl) = last_syl {
                let trim = self.committed_pinyin_buf.len().saturating_sub(syl.len());
                self.committed_pinyin_buf.truncate(trim);
                // Prepend the syllable back to buffer.
                self.buffer = format!("{syl}{}", self.buffer);
                self.raw_buffer = format!("{syl}{}", self.raw_buffer);
            }
            self.preedit = format!("{}{}", self.committed_text, self.raw_buffer);
            self.cursor = self.preedit.len();
            self.candidates_fresh = false;
            return self.query_pinyin(env);
        }

        self.buffer.pop();
        self.raw_buffer.pop();
        self.preedit = format!("{}{}", self.committed_text, self.raw_buffer);
        self.cursor = self.preedit.len();
        self.candidates_fresh = false;
        if self.buffer.is_empty() {
            self.reset();
            ImeView::empty()
        } else {
            self.query_pinyin(env)
        }
    }

    fn pinyin_enter(&mut self) -> ImeView {
        // Enter 强选 raw 文本:提交原始大小写(raw_buffer),非小写 buffer。
        let raw = std::mem::take(&mut self.raw_buffer);
        let committed = std::mem::take(&mut self.committed_text);
        let text = if committed.is_empty() {
            raw
        } else {
            format!("{committed}{raw}")
        };
        self.reset();
        Self::commit_view(&text)
    }

    fn pinyin_space(&mut self, env: &dyn StepEnv) -> ImeView {
        if !self.candidates_fresh {
            // No candidates — commit raw (committed_text + raw_buffer)。
            let committed = std::mem::take(&mut self.committed_text);
            let raw = std::mem::take(&mut self.raw_buffer);
            let _ = std::mem::take(&mut self.buffer);
            self.candidates.clear();
            self.state = ComposeState::Idle;
            let text = if committed.is_empty() {
                raw
            } else {
                format!("{committed}{raw}")
            };
            self.candidates_fresh = false;
            return Self::commit_view(&text);
        }

        // Fresh candidates: commit the highlighted one.
        let idx = self
            .candidate_highlight
            .min(self.candidates.len().saturating_sub(1));
        // Delegate to select() — it handles full vs partial commit correctly.
        self.candidates_fresh = false;
        self.select(idx, env)
    }

    fn pinyin_terminator(&mut self, ch: char) -> ImeView {
        let fresh = self.candidates_fresh;
        let top = self.candidates.first().cloned();
        let committed = std::mem::take(&mut self.committed_text);
        let raw = std::mem::take(&mut self.raw_buffer);
        let _ = std::mem::take(&mut self.buffer);
        self.candidates_fresh = false;
        self.state = ComposeState::Idle;
        self.candidates.clear();

        let prefix = if committed.is_empty() {
            String::new()
        } else {
            committed
        };
        if !fresh {
            return Self::commit_view(&format!("{prefix}{raw}{ch}"));
        }
        let text = match top {
            Some(t) => format!("{prefix}{}{ch}", apply_input_casing(&t, &raw)),
            None => format!("{prefix}{raw}{ch}"),
        };
        Self::commit_view(&text)
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

/// 提交英文候选时,把用户键入的大小写回填到词典(小写)单词上。
///
/// `word` 是候选文本(词典小写,如 "english"),`raw_input` 是当前未提交
/// 输入的原始大小写([`StateMachine::raw_buffer`])。仅当 `word` 的小写形式
/// 以 `raw_input` 的小写形式为前缀时,逐字符回填前缀的大小写;余下部分
/// (用户没打完、由词典补全的段)保持词典小写。汉字等非 ASCII 候选天然
/// no-op("好".starts_with("hao") 为 false)。
///
/// ```text
/// "Engli" + "english" → "English"   (前缀回填 + 补全段小写)
/// "ENGLISH" + "english" → "ENGLISH"
/// "english" + "english" → "english"
/// "hao" + "好" → "好"               (no-op)
/// ```
pub(crate) fn apply_input_casing(word: &str, raw_input: &str) -> String {
    if raw_input.is_empty() || word.is_empty() {
        return word.to_string();
    }
    // 仅 ASCII 字母参与大小写回填(拼音/英文输入);含非字母(raw 里混入
    // 符号)时保守不处理。
    if !raw_input.chars().all(|c| c.is_ascii_alphabetic()) {
        return word.to_string();
    }
    // 用户全小写 → 保留词典原始大小写(如 iPhone)。只有用户明确打了
    // 大写才用键入的大小写覆盖前缀。
    if !raw_input.chars().any(|c| c.is_ascii_uppercase()) {
        return word.to_string();
    }
    let word_lower = word.to_ascii_lowercase();
    let raw_lower = raw_input.to_ascii_lowercase();
    if !word_lower.starts_with(&raw_lower) {
        return word.to_string();
    }

    let mut out = String::with_capacity(word.len());
    let mut word_chars = word.chars();
    for rc in raw_input.chars() {
        match word_chars.next() {
            Some(wc) if wc.is_ascii_alphabetic() => out.push(rc),
            Some(wc) => out.push(wc),
            None => break,
        }
    }
    out.extend(word_chars);
    out
}

/// Borrowed engine components needed by the FSM to evaluate transitions.
pub trait StepEnv {
    //(InputContext 经全路径引用;trait 方法签名保持与家族 API 一致)
    fn matcher(&self) -> &Matcher;
    fn expander(&self) -> &Expander;

    /// Unified candidate scorer — combines all families.
    fn scorer(&self) -> &crate::family::UnifiedScorer;

    /// Extract the first valid pinyin syllable from the input.
    /// (纯函数:最长合法音节前缀,见 `family::pinyin::first_syllable_of`。)
    fn first_syllable(&self, pinyin: &str) -> Option<String> {
        crate::family::pinyin::first_syllable_of(pinyin)
    }

    /// 造词单字候选(逐字输入组词)—— 拼音家族私有能力(D5):链式豁免/
    /// 多音节判定/首音节提取/单字过滤在家族内聚,壳只管调用与面板重排。
    /// 默认空(无拼音家族的环境)。
    fn compose_single_chars(
        &self,
        input: &str,
        ctx: &crate::family::InputContext,
        existing: &[String],
        limit: usize,
    ) -> Vec<crate::family::ScoredCandidate> {
        Vec::new()
    }

    /// Record a user pick in inputx-pinyin's L0 layer for frequency boosting.
    fn record_pick(&self, pinyin: &str, word: &str);

    /// Called after a multi-step composition completes.
    fn learn_phrase(&self, pinyin: &str, hanzi: &str);

    /// Called after a composed (自生词) multi-step selection completes — the
    /// result joins the phrase book unconditionally.
    fn learn_composed_phrase(&self, pinyin: &str, hanzi: &str);

    /// The magic command registry — spawns live member instances on trigger
    /// completion, holds the shared resources (voice slot, req config).
    fn magic(&self) -> &crate::family::magic::MagicFamily;

    /// 向 IoThread 上的 voice server 发命令(`#asr` 用它发 `Attach`/`Detach`)。
    /// 默认委托 `magic()` 的共享槽 —— `Dispatcher` 无需额外字段。
    fn voice_cmd_tx(&self) -> Option<crate::io_thread::VoiceCmdSender> {
        self.magic().voice_cmd_tx()
    }
}

/// 单字母输入:字母本尊 + 大小写互换置顶(english family 的 self/case 成员)。
/// 位置规则而非分数竞争 —— 跨家族打分下 english(×priority 0.70)赢不了
/// 拼音单音节(如 啊 ~0.86),但单字母的意图几乎总是字母本身。
///
/// `query_pinyin`(状态机候选)与 `candidates_detailed`(engine 层重排镜像)
/// 共用此规则,保持两处候选顺序一致。多字符 buffer 原样返回。
pub(crate) fn promote_single_letter(
    buffer: &str,
    mut ranked: Vec<crate::family::RankedCandidate>,
) -> Vec<crate::family::RankedCandidate> {
    if buffer.len() != 1 {
        return ranked;
    }
    let Some(ch) = buffer
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphabetic())
    else {
        return ranked;
    };
    let (lower, upper) = (ch.to_string(), ch.to_ascii_uppercase().to_string());
    let is_letter = |r: &crate::family::RankedCandidate| {
        r.family == "english" && (r.text == lower || r.text == upper)
    };
    let mut head: Vec<_> = ranked.iter().filter(|r| is_letter(r)).cloned().collect();
    head.sort_by_key(|r| if r.text == lower { 0 } else { 1 });
    if head.is_empty() {
        return ranked;
    }
    ranked.retain(|r| !is_letter(r));
    head.extend(ranked);
    head
}

#[cfg(test)]
mod tests {
    use super::apply_input_casing;

    #[test]
    fn all_lowercase_input_preserves_dict_case() {
        // 用户全小写 → 保留词典原始大小写(专有名词 iPhone)。
        assert_eq!(apply_input_casing("iPhone", "iphone"), "iPhone");
        assert_eq!(apply_input_casing("NASA", "nasa"), "NASA");
        assert_eq!(apply_input_casing("english", "english"), "english");
    }

    #[test]
    fn typed_uppercase_overrides_dict_case() {
        assert_eq!(apply_input_casing("iPhone", "IPHONE"), "IPHONE");
        assert_eq!(apply_input_casing("english", "English"), "English");
        assert_eq!(apply_input_casing("iPhone", "iPhone"), "iPhone");
    }

    #[test]
    fn prefix_case_applied_to_completion_suffix() {
        // 补全段(用户没打的)保持词典原始大小写;键入前缀用用户大小写。
        assert_eq!(apply_input_casing("iPhone", "Iph"), "Iphone");
        assert_eq!(apply_input_casing("english", "Engli"), "English");
    }

    #[test]
    fn non_ascii_and_unrelated_are_noop() {
        assert_eq!(apply_input_casing("好", "hao"), "好");
        assert_eq!(apply_input_casing("英语", "yingyu"), "英语");
        // 候选与输入无前缀关系 → 不动。
        assert_eq!(apply_input_casing("hello", "world"), "hello");
        // 空输入 → 不动。
        assert_eq!(apply_input_casing("iPhone", ""), "iPhone");
    }
}
