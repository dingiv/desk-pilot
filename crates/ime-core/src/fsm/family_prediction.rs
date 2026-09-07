//! family — 第一路键处理:FamilyPrediction(打分家族预测,round13)。
//!
//! 双路键处理之一:`#` 之外的普通键进这里,做拼音/英文/emoji 家族预测。
//! 本模块**不依赖 ControlPane** —— 只吃 [`SessionState`](会话纯数据);
//! 三个家族对象自身持有状态(pinyin/english/emoji Arc,跨会话共享),
//! 由引擎持有,经 [`StepEnv`] 注入。
//!
//! 路径:收集(`collect_pinyin` 产出交付件)→ 路由层转 stage3 →
//! [`apply_post_outcome`] 落位 → `render` 出视图。
//!
//! | 组合状态 | 键 | 迁移 | 视图 |
//! |---|---|---|---|
//! | Idle      | `#`/`/`    | Snippet  | (转第二路 MagicFlow)       |
//! | Idle      | a-z        | Pinyin   | collect + render           |
//! | Pinyin    | a-z        | Pinyin   | extend + render            |
//! | Pinyin    | Space      | Idle     | commit_text                |
//! | Pinyin    | Enter      | Idle     | commit_text                |
//! | Pinyin    | Backspace  | P/Idle   | pop + render               |
//! | Pinyin    | other      | Idle     | commit_text                |
//!

use crate::frontend::ImeView;
use super::key::KeyKind;
use super::control::SessionState;
use super::post::{commit_view, passthrough_view, render, CandMeta};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposeState {
    #[default]
    Idle,
    Snippet,
    Pinyin,
}


/// 组合会话(S4 状态下沉):一次输入组合的文本状态 —— 原始键入、预测串、
/// 预编辑展示、光标、造词半成品。生命周期:idle 起步,提交/重置终。
/// 不变式:`buffer` 是 `raw_buffer` 的 ASCII 小写,二者等长。
#[derive(Debug, Default)]
pub(crate) struct Composition {
    /// 键入的原始文本(保留大小写)。英文候选提交时按它回填大小写
    /// (English 而非 english)。
    pub raw_buffer: String,
    /// 剩余未提交的拼音/字母串(小写;预测输入)。
    pub buffer: String,
    /// 展示预编辑:committed 汉字 + 剩余拼音。
    pub preedit: String,
    /// preedit 内的光标字节偏移。
    pub cursor: usize,
    /// 造词半成品:逐字选择期间已提交的汉字(如 "李正")。
    pub committed_text: String,
    /// 已提交部分对应的拼音(如 "lizheng")。
    pub committed_pinyin: String,
}

impl Composition {
    /// 展示预编辑同步(round11 消重):`preedit = committed + raw`,
    /// 光标到尾。拼音组合每次文本变化(入字/退格/逐字提交)后调用。
    pub(crate) fn sync_preedit(&mut self) {
        self.preedit = format!("{}{}", self.committed_text, self.raw_buffer);
        self.cursor = self.preedit.len();
    }
}

/// 候选面板(S4 状态下沉):items/meta/partial 三列表同源同序
/// (PanelItem 单点派生),加高亮/分页/窗口配置 —— 面板展示的全部状态
/// 内聚一处;fill_view 的滑动窗口、move_highlight/change_page、select 的
/// 全局序判定都是面板行为。
#[derive(Debug, Default)]
pub(crate) struct CandidatePanel {
    /// 全量候选(merged;view 装其滑动窗口)。
    pub items: Vec<String>,
    /// 与 items 同序的元数据(fill_view meta / select 家族判定)。
    pub meta: Vec<CandMeta>,
    /// 与 items 同序的部分提交标记(造词单字区,">")。
    pub partial: Vec<bool>,
    /// 词头数(UI 显示偏移兼容)。
    pub full_comp_count: usize,
    /// 全局高亮(merged 序)。
    pub highlight: usize,
    /// 当前页(0-based;highlight 推导或翻页键设置)。
    pub page: usize,
    /// 每页条数(yaml page_size 构造注入)。
    pub page_size: usize,
    /// 候选是否与 buffer 同步(缓存标记)。
    pub fresh: bool,
}

impl SessionState {
    /// 面板镜像(S3 统一):last_meta → RankedCandidate,与 candidates
    /// 同序同源 —— 用户看见什么,这里就是什么。
    ///
    /// round12 规范化:Snippet 会话分支(命令预测 / 补全不经 scorer)
    /// 从 engine 壳下沉至此 —— 会话语义归 stage2,壳只调门面。
    pub(crate) fn detailed(&self) -> Vec<crate::family::RankedCandidate> {
        // Snippet 态(命令组合):candidates 来自命令预测 / 补全,不是 scorer。
        // 直接返回面板,让 #asr 语音 / 命令补全提示正确显示。
        if self.state == ComposeState::Snippet && self.panel.fresh {
            let family: &'static str = self
                .magic
                .active
                .as_ref()
                .map(|m| if m.name().is_empty() { "snippet" } else { "magic" })
                .unwrap_or("magic");
            return self
                .panel
                .items
                .iter()
                .map(|c| crate::family::RankedCandidate {
                    text: c.clone(),
                    score: 1.0,
                    family,
                    source: "exact",
                })
                .collect();
        }
        self.panel.meta
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

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("state", &self.state)
            .field("buffer", &self.comp.buffer)
            .field("preedit", &self.comp.preedit)
            .field("cursor", &self.comp.cursor)
            .field("panel.items", &self.panel.items)
            .field("panel.highlight", &self.panel.highlight)
            .field("panel.page", &self.panel.page)
            .field(
                "active_command",
                &self.magic.active.as_ref().map(|m| m.name()),
            )
            .finish()
    }
}

// FIXME: family.rs 为什么会有状态机的结构体实现? 保持单向依赖
impl SessionState {
    /// Construct with a configurable candidate page size (default 7).
    /// The engine passes `swift-ime.yaml → input.page_size` via
    /// [`ImeEngine::set_page_size`](crate::engine::ImeEngine::set_page_size).
    pub fn with_page_size(
        page_size: u32,
        wordbook: std::sync::Arc<crate::store::wordbook::WordBook>,
    ) -> Self {
        SessionState {
            wordbook,
            panel: CandidatePanel {
                page_size: page_size.max(1) as usize,
                ..CandidatePanel::default()
            },
            ..SessionState::default()
        }
    }

    /// stage2 键入口:控制键以 [`KeyKind`] 枚举分流(键语义不再伪装成
    /// 字符 —— 旧实现 stage1 编码 `' '`/`'\n'`/`'\x08'`、stage2 再解码,
    /// 两处字面量一致才正确;现在键是键,字符是字符)。
    pub fn step_key(&mut self, key: KeyKind, env: &dyn StepEnv) -> ImeView {
        match self.state {
            // idle 的控制键属于应用(stage1 已放行;防御兜底)。
            ComposeState::Idle => passthrough_view(),
            ComposeState::Snippet => self.snippet_key(key, env),
            ComposeState::Pinyin => self.pinyin_key(key, env),
        }
    }

    /// 文本字符通道(字母/数字/符号/触发符):idle 内自分流。
    pub fn step_char(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        match self.state {
            ComposeState::Idle => self.handle_idle(ch, env),
            ComposeState::Snippet => self.snippet_char(ch, env),
            ComposeState::Pinyin => self.pinyin_char(ch, env),
        }
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
        let picked = self.panel.items.get(index).cloned().unwrap_or_default();
        if picked.is_empty() {
            return self.make_view();
        }

        let is_partial = self.panel.partial.get(index).copied().unwrap_or(false);
        if !is_partial {
            // Full commit: combine committed_text + selected text. 英文候选按
            // 键入的原始大小写回填(raw_buffer),汉字候选天然 no-op。
            let picked_cased = apply_input_casing(&picked, &self.comp.raw_buffer);
            let final_text = if self.comp.committed_text.is_empty() {
                picked_cased.clone()
            } else {
                format!("{}{}", self.comp.committed_text, picked_cased)
            };
            let full_pinyin = if self.comp.committed_text.is_empty() {
                self.comp.buffer.clone()
            } else {
                format!("{}{}", self.committed_pinyin(), self.comp.buffer)
            };
            // 提交候选的来源家族 —— 两个家族的单词本各自闭环:
            // 拼音提交 → 拼音 L0/单词本;英文提交 → 英文家族(且词典词不学)。
            let commit_family = self
                .panel
                .meta
                .iter()
                .find(|m| m.text == picked)
                .map(|m| m.family);
            // round14:stage2 只产出学习回执(纯数据),不调学习接口 ——
            // 分发结算在后处理(post::learn_commit),家族不感知 commit。
            // L0 频率加成只对拼音族提交生效;自生词(逐字选择后整体提交)
            // 无条件入本;直接空格选 top **不学**(decomp 会被 Viterbi 重组)。
            if commit_family == Some("pinyin") {
                self.pending_learning
                    .push(crate::fsm::post::CommitReceipt::PinyinPick {
                        pinyin: full_pinyin.clone(),
                        word: final_text.clone(),
                    });
            }
            if !self.comp.committed_text.is_empty() {
                self.pending_learning
                    .push(crate::fsm::post::CommitReceipt::ComposedPhrase {
                        pinyin: full_pinyin,
                        text: final_text.clone(),
                    });
            }
            self.reset();
            self.commit_text(&final_text, commit_family);
            commit_view(&final_text)
        } else {
            // Partial commit: append this single character, shrink buffer.
            self.comp.committed_text.push_str(&picked);
            let first_syl = env.first_syllable(&self.comp.buffer).unwrap_or_default();
            let first_len = first_syl.len();
            if first_len > 0 && first_len <= self.comp.buffer.len() {
                // 逐字选择的 L0 记录(round14:回执化,后处理结算)。
                let consumed = self.comp.buffer[..first_len].to_string();
                self.pending_learning
                    .push(crate::fsm::post::CommitReceipt::PinyinPick {
                        pinyin: consumed.clone(),
                        word: picked.clone(),
                    });
                self.comp.committed_pinyin.push_str(&consumed);
                self.comp.buffer = self.comp.buffer[first_len..].to_string();
                // 同步收缩 raw_buffer(consumed 是小写音节,等字节长)。
                self.comp.raw_buffer = self.comp.raw_buffer[first_len..].to_string();
            }
            self.comp.sync_preedit();
            self.panel.fresh = false;
            self.panel.highlight = 0;
            self.query_pinyin(env)
        }
    }

    /// 提交落地的**统一出口**:context 滚动 + 学习记录(recency/bigram/
    /// 自生词,经 [`FamilyEnv`])。提交语义属于管线内部 —— engine 壳对
    /// 提交零回写(原 `key_ctx`/`select_ctx` 的"太靠外"回写已内聚于此)。
    ///
    /// `family` = 提交内容的来源家族:`Some("english")` 只进英文 recency、
    /// 不学自生词;`None`(raw 强选 / snippet 文本)按拼音族 recency、
    /// ASCII 时学英文自生词 —— 与旧引擎回写行为一致。
    pub(crate) fn commit_text(&mut self, text: &str, family: Option<&'static str>) {
        self.chain_anchor = None;
        self.context.update(text);
        // round14:学习回执化 —— recency 分流 / ASCII 自生词 / 长度统计
        // 的策略与分发都在后处理(post::learn_commit)。
        self.pending_learning
            .push(crate::fsm::post::CommitReceipt::Commit {
                text: text.to_string(),
                family,
            });
    }

    /// 强提文本并结束组合(round11 统一出口):reset 全部会话态 → 提交 →
    /// COMMIT 视图。适用一切"绕过候选直接上屏"的路径 —— Enter 原文强选、
    /// rollback、未知命令提交、拼音符号终结、空格无候选提交。调用方负责
    /// 先拼好文本(含 committed+raw 拼接与大小写回填),reset 由这里统一做
    /// (含 active 命令清理)。
    pub(crate) fn commit_raw_and_reset(&mut self, text: &str) -> ImeView {
        self.reset();
        self.commit_text(text, None);
        commit_view(text)
    }

    // ── 状态派生与 stage1 门面(round11:自 state.rs 迁入)─────────────

    /// 从组合状态机提取状态标志位(路由前查表、路由后同步都用这里)。
    pub fn state_flags(&self) -> crate::fsm::key::StateFlags {
        use crate::fsm::key::StateFlags;
        let mut f = StateFlags::empty();
        if self.state != ComposeState::Idle || !self.comp.buffer.is_empty() {
            f |= StateFlags::COMPOSING;
        }
        if !self.panel.items.is_empty() {
            f |= StateFlags::PANEL_OPEN;
        }
        match self.state {
            ComposeState::Idle => {}
            ComposeState::Pinyin => f |= StateFlags::PINYIN,
            ComposeState::Snippet => f |= StateFlags::SNIPPET,
        }
        if !self.comp.committed_text.is_empty() {
            f |= StateFlags::WORD_BUILDING;
        }
        if self.has_pending_choices() {
            f |= StateFlags::PENDING;
        }
        f
    }

    /// 是否有"待确认"的选项(命令预测 / 补全提示)。
    fn has_pending_choices(&self) -> bool {
        !self.magic.predictions.is_empty() || !self.magic.hints.is_empty()
    }

    /// 翻页(方向翻页键 / `-` `+`):页号 clamp,高亮跳到新页首。
    pub fn change_page(&mut self, delta: i32) {
        let n = self.panel.items.len();
        if n == 0 || self.panel.page_size == 0 {
            return;
        }
        let total_pages = n.div_ceil(self.panel.page_size);
        if total_pages <= 1 {
            return;
        }
        let new_page =
            (self.panel.page as i32 + delta).clamp(0, total_pages as i32 - 1) as usize;
        if new_page != self.panel.page {
            self.panel.page = new_page;
            self.panel.highlight = new_page * self.panel.page_size;
            self.sync_magic_preedit();
        }
    }

    /// **stage1 门面**:页内数字选词(`1..9` 按**当前页内**序号换算全局序)。
    /// 越界(页内无此序号)返回透传 —— idle 的裸数字属于应用。
    pub fn select_page_digit(&mut self, d: usize, env: &dyn StepEnv) -> ImeView {
        let base = self.panel.page.saturating_mul(self.panel.page_size);
        let idx = base + d.saturating_sub(1);
        if self.panel.items.len() > idx {
            self.select(idx, env)
        } else {
            passthrough_view()
        }
    }

    /// **stage1 门面**:preedit 光标左右移动(`[` / `]`),clamp 内聚于
    /// 组合会话 —— stage1 不触碰 `comp` 内部字段。
    ///
    /// 语义保持历史行为:字节偏移步进,右界按 chars 数(混合标准是历史
    /// quirk,行为不变原则下保留)。
    pub fn nudge_cursor(&mut self, delta: i32) {
        if delta < 0 {
            self.comp.cursor = self
                .comp
                .cursor
                .saturating_sub(delta.unsigned_abs() as usize);
        } else {
            let max = self.comp.preedit.chars().count();
            if self.comp.cursor < max {
                self.comp.cursor = (self.comp.cursor + delta as usize).min(max);
            }
        }
    }

    /// 页大小(round12:stage2 门面)—— 面板是会话状态,唯一写口在此。
    /// Build a view from the current state (no key processed). Used by the state
    /// machine itself and by magic members rendering their candidates.
    pub(crate) fn make_view(&self) -> ImeView {
        let mut v = render(&self.panel_snapshot());
        v.action = crate::frontend::action::HANDLED;
        v
    }

    /// 只读快照视图(round12 规范化):与 [`Self::make_view`] 同源
    /// `render`(含翻页窗口 / partial 标 / meta / preedit 转义),但不设
    /// action —— 供 engine `view()` 等被动查询出口复用,禁止在壳里手工
    /// 拼装 ImeView。
    pub(crate) fn snapshot_view(&self) -> ImeView {
        render(&self.panel_snapshot())
    }

    /// 组装只读面板快照(交付件 3:stage2 → 渲染)。
    pub(crate) fn panel_snapshot(&self) -> crate::fsm::post::PanelSnapshot {
        crate::fsm::post::PanelSnapshot {
            items: self.panel.items.clone(),
            meta: self.panel.meta.clone(),
            partial: self.panel.partial.clone(),
            highlight: self.panel.highlight,
            page: self.panel.page,
            page_size: self.panel.page_size,
            preedit: self.comp.preedit.clone(),
            cursor: self.comp.cursor,
            state: self.state,
            raw_buffer: self.comp.raw_buffer.clone(),
            buffer: self.comp.buffer.clone(),
            candidate_meta: self.candidate_meta_enabled,
        }
    }

    /// 待提交文本(round12 规范化):候选首条按键入原始大小写回填,
    /// 无候选时回退原始缓冲。应用侧失焦前的 pending commit 语义单点。
    pub(crate) fn pending_commit_text(&self) -> String {
        crate::fsm::post::pending_commit_text(&self.panel_snapshot())
    }

    pub fn set_page_size(&mut self, page_size: usize) {
        self.panel.page_size = page_size;
    }

    /// 候选 meta 调试开关(round12:stage2 门面,壳不直写字段)。
    pub fn set_candidate_meta(&mut self, on: bool) {
        self.candidate_meta_enabled = on;
    }

    pub fn reset(&mut self) {
        self.chain_flow.clear();
        self.chain_anchor = None;
        self.clear_active_command();
        self.state = ComposeState::Idle;
        self.comp.buffer.clear();
        self.comp.raw_buffer.clear();
        self.comp.preedit.clear();
        self.comp.cursor = 0;
        self.panel.items.clear();
        self.panel.highlight = 0;
        self.panel.page = 0;
        self.panel.fresh = false;
        self.comp.committed_text.clear();
        self.comp.committed_pinyin.clear();
        self.panel.full_comp_count = 0;
        self.panel.partial.clear();
        self.magic.hints.clear();
        self.magic.predictions.clear();
        self.magic.selectable = false;
    }

    /// Is the candidate panel OPEN (non-empty candidate list)? Navigation/paging special keys
    /// only act while it's open; when closed they pass through to the application.
    pub fn candidate_panel_open(&self) -> bool {
        !self.panel.items.is_empty()
    }

    pub fn move_highlight(&mut self, delta: i32) {
        if self.panel.items.is_empty() {
            return;
        }
        let new = (self.panel.highlight as i32 + delta)
            .clamp(0, self.panel.items.len() as i32 - 1) as usize;
        self.panel.highlight = new;
        if self.panel.page_size > 0 {
            self.panel.page = (new as u32)
                .checked_div(self.panel.page_size as u32)
                .unwrap_or(0) as usize;
        }
        // 魔法命令预测:应用高亮(将提交)跟随高亮移动。
        self.sync_magic_preedit();
    }

    /// Full pinyin for the committed portion.
    fn committed_pinyin(&self) -> String {
        self.comp.committed_pinyin.clone()
    }

    // ── Idle ───────────────────────────────────────────────────────────

    fn handle_idle(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        if env.magic().is_trigger_start(ch) {
            self.state = ComposeState::Snippet;
            self.comp.buffer.push(ch);
            self.comp.preedit = self.comp.buffer.clone();
            self.comp.cursor = 1;
            return self.make_view();
        }
        if ch.is_ascii_alphabetic() {
            // 大写字母视作小写进行预测(English → english),展示与提交
            // 保留原始大小写(raw_buffer)。
            self.state = ComposeState::Pinyin;
            self.comp.buffer.push(ch.to_ascii_lowercase());
            self.comp.raw_buffer.push(ch);
            self.comp.preedit = self.comp.raw_buffer.clone();
            self.comp.cursor = self.comp.preedit.len();
            self.panel.fresh = false;
            return self.query_pinyin(env);
        }
        passthrough_view()
    }

    // ── Snippet ────────────────────────────────────────────────────────

    /// Snippet 态:所有 `#…` 输入统一在此处理。
    ///
    /// - Backspace 删字符重查;Enter 强选原始文本;
    /// - Space 选中高亮候选(预测提交 / 补全改写 / rollback 提交);
    /// - 数字键在可选中态(精确无参 / 前缀)选中候选,否则作为命令文本;
    /// - 其它字符追加后重查。
    fn snippet_key(&mut self, key: KeyKind, env: &dyn StepEnv) -> ImeView {
        match key {
            // Backspace: pop last char, re-query. Empty → reset.
            KeyKind::Backspace => {
                self.comp.buffer.pop();
                if self.comp.buffer.is_empty() {
                    self.reset();
                    return ImeView::empty();
                }
                self.query_magic(env)
            }
            // Enter: force raw text.
            KeyKind::Enter => {
                let raw = std::mem::take(&mut self.comp.buffer);
                self.commit_raw_and_reset(&raw)
            }
            // Space: commit the highlighted candidate.
            KeyKind::Space => {
                let hl = self
                    .panel.highlight
                    .min(self.panel.items.len().saturating_sub(1));
                self.select_magic(hl, env)
            }
            _ => passthrough_view(),
        }
    }

    /// Snippet 态的文本字符:数字在可选中态选候选、否则与其它字符一并追加
    /// 进命令缓冲(`?num=2` 的数字与 `-`/`[`/`]` 由 stage1 经
    /// `as_command_char` hoist 到本通道)。
    fn snippet_char(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        // 数字键:可选中时选中候选,否则作为命令文本追加(如 `?num=2`)。
        if let d @ '1'..='9' = ch {
            if self.magic.selectable {
                let idx = (d as u8 - b'1') as usize;
                if idx < self.panel.items.len() {
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
        self.comp.buffer.push(ch);
        self.query_magic(env)
    }

    // ── Pinyin ─────────────────────────────────────────────────────────

    /// Pinyin 态的控制键(枚举分流):退格删拼音、回车/空格强选。
    fn pinyin_key(&mut self, key: KeyKind, env: &dyn StepEnv) -> ImeView {
        match key {
            KeyKind::Backspace => self.pinyin_backspace(env),
            KeyKind::Enter => self.pinyin_enter(),
            KeyKind::Space => self.pinyin_space(env),
            _ => passthrough_view(),
        }
    }

    /// Pinyin 态的文本字符:字母入 buffer、`'` 切链、`'#` 开命令链、
    /// 其余符号是终结符。
    fn pinyin_char(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        // 链分隔符:`'` 是组合内结构字符(ti'an 的两条链),不是终结符。
        // 追加进 buffer;预测层(拼音家族)按 `'` 切链组合。回格删 `'`
        // 天然回到无链状态 —— 链结构纯由 buffer 内容决定,无隐藏状态。
        // 附带"我说完了"信号:语音会话在听时让 aura 立即归档开放窗口
        // (整窗 batch,跳过 merge_gap 等待);无语音会话 → tx 为 None,跳过。
        if ch == '\'' {
            // round22 链式高亮锚点:分链那一刻捕获面板高亮词(供
            // `yibu'#freq/up` 类上下文命令定位操作对象)。
            if let Some(word) = self
                .panel
                .items
                .get(self.panel.highlight)
                .filter(|w| !w.is_empty())
            {
                self.chain_anchor = Some(word.clone());
            }
            if let Some(tx) = env.voice_cmd_tx() {
                tx.send(crate::io_thread::VoiceCmd::FlushParagraph);
            }
            self.comp.buffer.push('\'');
            self.comp.raw_buffer.push('\'');
            self.comp.sync_preedit();
            self.panel.fresh = false;
            return self.query_pinyin(env);
        }
        // 链式命令:`'#` 序列开启命令链(X'#translate)。`#` 不终结组合、
        // 不提交 —— 上游链保留在 buffer 里,转入 Snippet 态做命令输入;
        // 单独的 `#`(无 `'` 前导)维持旧的终结符行为(提交候选 + `#`)。
        if ch == '#' && self.comp.buffer.ends_with('\'') {
            self.comp.buffer.push('#');
            self.comp.raw_buffer.push('#');
            self.state = ComposeState::Snippet;
            self.comp.sync_preedit();
            self.panel.fresh = false;
            return self.query_magic(env);
        }
        if ch.is_ascii_alphabetic() {
            self.comp.buffer.push(ch.to_ascii_lowercase());
            self.comp.raw_buffer.push(ch);
            self.comp.sync_preedit();
            self.panel.fresh = false;
            return self.query_pinyin(env);
        }
        self.pinyin_terminator(ch)
    }

    /// 便捷组合:收集 + 交路由层解析(stage3 编排仍发生在
    /// [`SessionState::resolve`],stage2 不直接调 stage3)。
    pub(crate) fn query_pinyin(&mut self, env: &dyn StepEnv) -> ImeView {
        let request = self.collect_pinyin(env);
        crate::fsm::pre::resolve(self, request, env)
    }

    /// 打分路径收集(round12):家族收集后产出交付件,不落位、不调 stage3
    /// —— 由路由层(SessionState::resolve)转 stage3 并交回回执。
    pub(crate) fn collect_pinyin(
        &mut self,
        env: &dyn StepEnv,
    ) -> Option<crate::fsm::post::PostRequest> {
        // ── Stage 2:家族收集(各家族独立预测 + top_n 预过滤,未合成)──
        let collected = env.scorer().collect(&self.comp.buffer, &self.context);
        Some(crate::fsm::post::PostRequest {
            buffer: self.comp.buffer.clone(),
            context: self.context.clone(),
            state: self.state,
            collected,
        })
    }

    /// 落位回执(round12):把 stage3 的 PostOutcome 写入面板(唯一写口),
    /// 三列表同源同序 —— fill_view 的窗口偏移 / select 的家族判定 /
    /// ">" 部分提交标记全从同一 PanelItem 序列出发。
    pub(crate) fn apply_post_outcome(&mut self, outcome: crate::fsm::post::PostOutcome) -> ImeView {
        self.panel.full_comp_count = outcome.full_comp_count;
        self.panel.items = outcome.items.iter().map(|i| i.text.clone()).collect();
        self.panel.partial = outcome.items.iter().map(|i| i.partial).collect();
        self.panel.meta = outcome.items.iter().map(|i| i.meta.clone()).collect();

        let cands = self.panel.items.clone();
        if !cands.is_empty() {
            self.panel.highlight = 0;
            self.panel.page = 0;
            self.panel.fresh = true;
        } else {
            self.panel.items.clear();
            self.panel.fresh = false;
        }
        self.make_view()
    }

    fn pinyin_backspace(&mut self, env: &dyn StepEnv) -> ImeView {
        // If we have committed text, backspace undoes the last committed char.
        if !self.comp.committed_text.is_empty() {
            self.comp.committed_text.pop();
            // Undo the last consumed syllable from committed_pinyin_buf.
            let last_syl = env.first_syllable(&self.comp.committed_pinyin);
            if let Some(syl) = last_syl {
                let trim = self.comp.committed_pinyin.len().saturating_sub(syl.len());
                self.comp.committed_pinyin.truncate(trim);
                // Prepend the syllable back to buffer.
                self.comp.buffer = format!("{syl}{}", self.comp.buffer);
                self.comp.raw_buffer = format!("{syl}{}", self.comp.raw_buffer);
            }
            self.comp.sync_preedit();
            self.panel.fresh = false;
            return self.query_pinyin(env);
        }

        self.comp.buffer.pop();
        self.comp.raw_buffer.pop();
        self.comp.sync_preedit();
        self.panel.fresh = false;
        if self.comp.buffer.is_empty() {
            self.reset();
            ImeView::empty()
        } else {
            self.query_pinyin(env)
        }
    }

    fn pinyin_enter(&mut self) -> ImeView {
        // Enter 强选 raw 文本:提交原始大小写(raw_buffer),非小写 buffer。
        let raw = std::mem::take(&mut self.comp.raw_buffer);
        let committed = std::mem::take(&mut self.comp.committed_text);
        let text = if committed.is_empty() {
            raw
        } else {
            format!("{committed}{raw}")
        };
        self.commit_raw_and_reset(&text)
    }

    fn pinyin_space(&mut self, env: &dyn StepEnv) -> ImeView {
        if !self.panel.fresh {
            // No candidates — commit raw (committed_text + raw_buffer)。
            let committed = std::mem::take(&mut self.comp.committed_text);
            let raw = std::mem::take(&mut self.comp.raw_buffer);
            let _ = std::mem::take(&mut self.comp.buffer);
            let text = if committed.is_empty() {
                raw
            } else {
                format!("{committed}{raw}")
            };
            return self.commit_raw_and_reset(&text);
        }

        // Fresh candidates: commit the highlighted one.
        let idx = self
            .panel.highlight
            .min(self.panel.items.len().saturating_sub(1));
        // Delegate to select() — it handles full vs partial commit correctly.
        self.panel.fresh = false;
        self.select(idx, env)
    }

    fn pinyin_terminator(&mut self, ch: char) -> ImeView {
        let fresh = self.panel.fresh;
        let top = self.panel.items.first().cloned();
        let committed = std::mem::take(&mut self.comp.committed_text);
        let raw = std::mem::take(&mut self.comp.raw_buffer);
        let _ = std::mem::take(&mut self.comp.buffer);

        let prefix = if committed.is_empty() {
            String::new()
        } else {
            committed
        };
        if !fresh {
            let text = format!("{prefix}{raw}{ch}");
            return self.commit_raw_and_reset(&text);
        }
        let text = match top {
            Some(t) => format!("{prefix}{}{ch}", apply_input_casing(&t, &raw)),
            None => format!("{prefix}{raw}{ch}"),
        };
        self.commit_raw_and_reset(&text)
    }
}

/// 提交英文候选时,把用户键入的大小写回填到词典(小写)单词上。
///
/// `word` 是候选文本(词典小写,如 "english"),`raw_input` 是当前未提交
/// 输入的原始大小写([`SessionState::raw_buffer`])。仅当 `word` 的小写形式
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
/// 状态机侧的环境接面(R4 依赖单向化):家族能力已上移
/// [`crate::family::FamilyEnv`](由 family 定义,本 trait 继承),此处只保留
/// fsm 特有的能力 —— 统一打分器与拼音首音节纯函数。
pub trait StepEnv: crate::family::FamilyEnv {
    /// Unified candidate scorer — combines all families.
    fn scorer(&self) -> &crate::family::UnifiedScorer;

    /// Stage3 候选过滤链(round10 W7 骨架):postprocess 在合成/置顶之后、
    /// PanelItem 化之前跑链。默认空链零成本直通;引擎侧经
    /// `ImeEngine::add_filter` 注册。
    fn filters(&self) -> &crate::fsm::post::FilterChain {
        &crate::fsm::post::EMPTY_FILTERS
    }

    /// Extract the first valid pinyin syllable from the input.
    /// (纯函数:最长合法音节前缀,见 `family::pinyin::first_syllable_of`。)
    fn first_syllable(&self, pinyin: &str) -> Option<String> {
        crate::family::pinyin::first_syllable_of(pinyin)
    }

    /// The magic command registry — spawns live member instances on trigger
    /// completion, holds the shared resources (voice slot, req config).
    fn magic(&self) -> &crate::family::magic::MagicFamily;
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

// ── 状态机 × StepEnv 交互测试(原 dispatcher.rs tests,转发层裁撤后迁此)──
#[cfg(test)]
mod step_env_tests {
    use super::*;
    use crate::family::magic::expander::StaticProvider;
    use crate::family::magic::MagicFamily;
    use crate::family::pinyin::PinyinFamily;
    use crate::family::UnifiedScorer;
    use crate::Expander;
    use std::sync::Arc;

    /// 轻量测试桩:内嵌组件的 StepEnv(原 new_for_test Dispatcher 的替身)。
    struct TestEnv {
        expander: Expander,
        scorer: UnifiedScorer,
        magic: MagicFamily,
    }

    impl TestEnv {
        fn new() -> Self {
            let magic = MagicFamily::new();
            magic.set_snippets(vec![crate::store::snippet_md::SnippetEntry {
                name: "greet".into(),
                comment: String::new(),
                params: Vec::new(),
                template: "你好,我是 AI 秘书".into(),
            }]);
            let pinyin = Arc::new(PinyinFamily::new());
            let scorer = UnifiedScorer::new(
                vec![Box::new(Arc::clone(&pinyin))],
                crate::family::scoring::FamilyPriorities::default(),
            );
            TestEnv {
                expander: Expander::new(Arc::new(StaticProvider {
                    date: "2026-07-23".into(),
                    clipboard: String::new(),
                })),
                scorer,
                magic,
            }
        }
        /// 模拟 stage1(pre)的分流:控制键走 step_key,字符走 step_char,
        /// snippet 态命令字符经 as_command_char hoist —— 与生产路径一致。
        fn process_key(&self, ch: char, sm: &mut SessionState) -> ImeView {
            let kind = KeyKind::from_char(ch);
            if sm.state == ComposeState::Snippet {
                if let Some(c) = kind.as_command_char() {
                    return sm.step_char(c, self);
                }
            }
            match kind {
                KeyKind::Space | KeyKind::Enter | KeyKind::Backspace => {
                    sm.step_key(kind, self)
                }
                KeyKind::Char(c) => sm.step_char(c, self),
                _ => passthrough_view(),
            }
        }
        fn select_candidate(&self, index: usize, sm: &mut SessionState) -> ImeView {
            sm.select(index, self)
        }
        fn reset(&self, sm: &mut SessionState) {
            sm.reset();
        }
        fn magic(&self) -> &MagicFamily {
            &self.magic
        }
    }

    impl crate::family::FamilyEnv for TestEnv {
        fn expander(&self) -> &Expander {
            &self.expander
        }
        fn record_pick(&self, _p: &str, _w: &str) {}
        fn learn_phrase(&self, _p: &str, _h: &str) {}
        fn learn_composed_phrase(&self, _p: &str, _h: &str) {}
    }

    impl StepEnv for TestEnv {
        fn scorer(&self) -> &UnifiedScorer {
            &self.scorer
        }
        fn magic(&self) -> &MagicFamily {
            &self.magic
        }
    }

    fn d() -> TestEnv {
        TestEnv::new()
    }

    fn sm() -> SessionState {
        SessionState::with_page_size(7, std::sync::Arc::new(
            crate::store::wordbook::WordBook::default(),
        ))
    }

    #[test]
    fn idle_slash_is_passthrough() {
        // 回归:is_trigger_start 曾把 '/' 也当触发器引导符,单独输入 '/'
        // 被误捕获进 snippet 态。原 matcher trie 根孩子只有 '#' —— 只有
        // '#' 进入命令组合;片段 '#/name' 的 '/' 是命令文本的一部分。
        let d = d();
        let mut s = sm();
        let v = d.process_key('/', &mut s);
        assert_eq!(s.state, super::ComposeState::Idle, "'/' stays idle");
        assert!(v.candidate_count == 0, "no candidates for '/'");
        // '#' 仍进入命令组合。
        let v2 = d.process_key('#', &mut s);
        assert_eq!(s.state, super::ComposeState::Snippet, "'#' enters snippet");
        let _ = v2;
    }

    #[test]
    fn idle_letter_enters_pinyin() {
        let d = d();
        let mut s = sm();
        let _v = d.process_key('n', &mut s);
        assert_eq!(
            s.state,
            super::ComposeState::Pinyin,
            "single letter should enter pinyin state"
        );
        // 'n' alone is not a complete syllable; candidates depend on FST/decomp.
        // Subsequent typing of 'i' should produce candidates.
        let v = d.process_key('i', &mut s);
        assert!(v.candidate_count > 0, "ni should produce candidates");
    }

    #[test]
    fn snippet_expansion() {
        let d = d();
        let mut s = sm();
        // Type #/greet — shows expansion as candidate, doesn't auto-expand.
        let mut view = ImeView::empty();
        for c in "#/greet".chars() {
            view = d.process_key(c, &mut s);
        }
        assert!(
            view.candidate_count > 0,
            "should show expansion as candidate, got {view:?}"
        );
        // Space commits the expansion.
        assert_eq!(
            ImeView::str_field(&d.process_key(' ', &mut s).commit_text),
            "你好,我是 AI 秘书"
        );
    }

    #[test]
    fn pinyin_space_commits_top() {
        let d = d();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert_eq!(
            ImeView::str_field(&d.process_key(' ', &mut s).commit_text),
            "你"
        );
    }

    #[test]
    fn pinyin_enter_commits_raw() {
        let d = d();
        let mut s = sm();
        d.process_key('h', &mut s);
        d.process_key('e', &mut s);
        d.process_key('l', &mut s);
        d.process_key('l', &mut s);
        d.process_key('o', &mut s);
        assert_eq!(
            ImeView::str_field(&d.process_key('\n', &mut s).commit_text),
            "hello"
        );
    }

    #[test]
    fn pinyin_and_snippet_coexist() {
        let d = d();
        let mut s = sm();
        // Type #/greet — 片段命令预测为模板展开,space commits。
        for c in "#/greet".chars() {
            d.process_key(c, &mut s);
        }
        assert_eq!(
            ImeView::str_field(&d.process_key(' ', &mut s).commit_text),
            "你好,我是 AI 秘书",
            "snippet commits expansion"
        );
        // After magic, typing letters enters pinyin.
        d.process_key('n', &mut s);
        let a = d.process_key('i', &mut s);
        assert!(
            a.candidate_count > 0,
            "after magic, ni should produce candidates, got {a:?}"
        );
    }

    #[test]
    fn select_candidate_commits_nth() {
        let d = d();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert_eq!(
            ImeView::str_field(&d.select_candidate(1, &mut s).commit_text),
            "呢"
        );
    }

    #[test]
    fn snippet_cursor_places_caret_in_expanded_text() {
        // Template with a mid-text $CURSOR marker: committing places the caret
        // at the marker's offset in the EXPANDED text (variables before it are
        // variable-length, so the offset is computed after expansion).
        use crate::family::magic::expander::{Expander, VariableProvider};
        use std::sync::Mutex;

        #[derive(Default)]
        struct MutableDate {
            date: Mutex<String>,
        }
        impl VariableProvider for MutableDate {
            fn resolve(&self, name: &str) -> Option<String> {
                match name {
                    "DATE" => Some(self.date.lock().unwrap().clone()),
                    _ => None,
                }
            }
        }

        let provider: std::sync::Arc<dyn VariableProvider> = std::sync::Arc::new(MutableDate {
            date: Mutex::new("2026-08-05".into()),
        });
        let mut d = TestEnv::new();
        d.expander = Expander::new(provider);
        d.magic()
            .set_snippets(vec![crate::store::snippet_md::SnippetEntry {
                name: "note".into(),
                comment: String::new(),
                params: Vec::new(),
                template: "$DATE 完成: $CURSOR 记得检查".into(),
            }]);
        let mut s = sm();
        for c in "#/note".chars() {
            d.process_key(c, &mut s);
        }
        let v = d.process_key(' ', &mut s);
        let text = ImeView::str_field(&v.commit_text);
        // "$DATE" = 10 bytes + " 完成: " = 9 → marker lands at byte 19.
        assert_eq!(
            text, "2026-08-05 完成:  记得检查",
            "marker removed from text"
        );
        assert_eq!(v.commit_cursor, 19, "caret mid-text, after the date prefix");
    }

    #[test]
    fn reset_clears_all() {
        let d = d();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.reset(&mut s);
        assert!(s.comp.buffer.is_empty());
        assert_eq!(s.state, super::ComposeState::Idle);
    }
}
