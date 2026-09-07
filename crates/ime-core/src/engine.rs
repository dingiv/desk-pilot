//! ImeEngine — the single integration point for all frontends.
//!
//! Manages per-context [`ControlPane`]s and
//! short-term [`InputContext`]. Supports both multi-context (fcitx5,
//! one engine per process) and single-context (mock, tests) usage.
//!
//! # Multi-context (fcitx5)
//!
//! ```ignore
//! let eng = ImeEngine::new();
//! eng.predict(ctx_ptr, 'n');
//! eng.select_candidate(ctx_ptr, 0);
//! eng.deactivate(ctx_ptr); // cleanup when window loses focus
//! ```
//!
//! # Single-context (tests / mock)
//!
//! ```ignore
//! let mut eng = ImeEngine::new();
//! for c in "nihao".chars() {
//!     eng.predict(KeyEvent::char(c));
//! }
//! eng.predict(KeyEvent::space());
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::family::magic::{MagicFamily, ReqFetcher};
use crate::fsm::event::{ControlEvent, ImeEvent};
use crate::fsm::family_prediction::StepEnv;
// 统一键事件由输入路由层定义(旧名 InputEvent;构造器同名,测试平移)。
use crate::frontend::ImeView;
use crate::fsm::control::{ControlPane, SessionState};
pub use crate::fsm::control::{KeyEvent, StateFlags};
use crate::store::snippet_md::*;
use crate::store::PersistenceManager;

// ── Session:引擎内的会话封装(round12,最小知道原则)─────────────────

/// 一个输入上下文 = 一台会话状态机(round12 S1:状态已全部上移,
/// `ControlPane` 同时是状态本体与事件入口)。引擎壳不得触碰其字段 ——
/// 一切动作经 [`Session::handle`](统一事件入口)与下列门面/只读转发
/// 方法。字段的访问权由本封装独占。
struct Session {
    pane: ControlPane,
}

impl Session {
    fn with_page_size(
        page_size: u32,
        candidate_meta: bool,
        wordbook: std::sync::Arc<crate::store::wordbook::WordBook>,
    ) -> Self {
        let mut pane = ControlPane::with_page_size(page_size, wordbook);
        pane.session.candidate_meta_enabled = candidate_meta;
        Session { pane }
    }

    /// 统一事件入口(键盘 / 控制 / 异步),见 [`ControlPane::handle_event`]。
    fn handle(&mut self, event: ImeEvent, env: &dyn StepEnv) -> Option<ImeView> {
        self.pane.handle_event(event, env)
    }

    /// 当前状态标志位镜像(状态栏 / 调试)。
    fn flags(&self) -> StateFlags {
        self.pane.flags()
    }

    /// 会话数据只读访问。
    fn session(&self) -> &SessionState {
        &self.pane.session
    }

    /// 会话数据可变访问(写路径:装配/配置)。
    fn session_mut(&mut self) -> &mut SessionState {
        &mut self.pane.session
    }

    // ── stage2 只读 / 门面转发(壳不得绕过)────────────────────────

    fn snapshot_view(&self) -> ImeView {
        self.session().snapshot_view()
    }

    fn pending_commit_text(&self) -> String {
        self.session().pending_commit_text()
    }

    fn buffer(&self) -> String {
        self.session().comp.buffer.clone()
    }

    fn candidates(&self) -> Vec<String> {
        self.session().panel.items.clone()
    }

    #[cfg(test)]
    fn last_meta(&self) -> Vec<crate::fsm::post::CandMeta> {
        self.session().panel.meta.to_vec()
    }

    fn detailed(&self) -> Vec<crate::family::RankedCandidate> {
        self.session().detailed()
    }

    /// #asr 会话探测(是否存活 / 调试串)。
    fn asr_probe(&self) -> (bool, String) {
        self.session().asr_probe()
    }

    /// magic tick 观测探针(state 调试串 / 是否有活跃成员)。
    fn tick_probe(&self) -> (String, bool) {
        (
            format!("{:?}", self.session().state),
            self.session().magic.active.is_some(),
        )
    }

    /// 释放活跃魔法成员(上下文销毁时)。
    fn clear_active_command(&mut self) {
        self.session_mut().clear_active_command();
    }

    /// 页大小(面板写入口在 stage2,会话代转)。
    fn set_page_size(&mut self, page_size: usize) {
        self.session_mut().set_page_size(page_size);
    }

    /// 候选 meta 调试开关(会话代转)。
    fn set_candidate_meta(&mut self, on: bool) {
        self.session_mut().set_candidate_meta(on);
    }
}

// ── ImeEngine ───────────────────────────────────────────────────────────

const DEFAULT_CTX: usize = 0; // used by single-context convenience methods

/// 默认 aura daemon origin。生产配置可在 `swift-ime.yaml → voice.aura_base` 覆盖。
pub const DEFAULT_VOICE_AURA_BASE: &str = "http://127.0.0.1:9091";

/// Self-contained IME engine. Manages the dispatcher, per-context state
/// machines, input context, and async waits.
pub struct ImeEngine {
    /// 变量展开器(snippet 模板 `$DATE` / `$CLIPBOARD` 等)。引擎必须持有:
    /// engine 本身就是 [`crate::family::FamilyEnv`] 接面的实现体,
    /// snippet 展开经 `env.expander()` 走这里(round12 结论:非冗余)。
    expander: crate::Expander,
    /// 统一打分器(家族容器 + 合成;合成段 S6 归 stage3 后处理)。
    scorer: crate::family::UnifiedScorer,
    /// 具体家族句柄(D5 接口隔离):学习/暖启/上下文开关等家族私有方法
    /// 直调,不经 trait 对象。scorer 持同一 Arc 当 trait 对象参与排序。
    pinyin_family: std::sync::Arc<crate::family::pinyin::PinyinFamily>,
    english_family: std::sync::Arc<crate::family::english::EnglishFamily>,
    emoji_family: std::sync::Arc<crate::family::emoji::EmojiFamily>,
    /// 单词本(round14):所有权归持久化模块(PersistenceManager 持本 Arc
    /// 的原始所有),引擎/SessionState/两家族只持引用克隆。
    wordbook: std::sync::Arc<crate::store::wordbook::WordBook>,
    /// The magic command registry — same `Arc` the scorer-side state uses. The engine
    /// routes late resource attachment (voice buffer, `#req` base/fetcher) here;
    /// the FSM spawns live member instances from it.
    magic: Arc<MagicFamily>,
    /// context management
    sessions: Mutex<HashMap<usize, Session>>,
    /// Unified persistence manager — owns the SQLite store and coordinates all
    /// user-model persistence (recency / bigrams / phrases / L0). `None` until
    /// [`init_store`](ImeEngine::init_store).
    persistence: Mutex<Option<PersistenceManager>>,
    /// 候选每页条数(swift-ime.yaml → input.page_size;默认 7)。传给每个新建的
    /// ControlPane —— 之前写死在 `ControlPane::new` 里(FIXME)。
    page_size: u32,
    /// 调试模式:候选词显示提供者与权重(swift-ime.yaml → debug.candidate_meta)。
    candidate_meta: bool,
    /// 前端句柄 —— 引擎 I/O 线程经它推送 UI 刷新 / 请求剪贴板。
    frontend: Arc<dyn crate::frontend::FrontEndHandle>,
    /// 单条 tokio I/O 线程(事件响应模型),预测主路径不建线程。
    io_thread: Arc<crate::io_thread::IoThread>,
    /// Stage3 候选过滤链(round10 W7):默认空链零成本直通;经
    /// `add_filter` 注册,postprocess 在合成/置顶之后跑链。
    filters: crate::fsm::post::FilterChain,
}

impl ImeEngine {
    /// Create a new engine with all default prediction families, built-in
    /// snippet triggers, and the embedded base phrase dictionary.
    pub fn new() -> Self {
        Self::with_pinyin_weights(crate::family::pinyin::PinyinWeights::default())
    }

    /// Create engine with custom pinyin family weights (from config file).
    /// voice listener 连接到 `127.0.0.1:9091`(默认 aura daemon origin)。
    pub fn with_pinyin_weights(weights: crate::family::pinyin::PinyinWeights) -> Self {
        Self::with_config(
            weights,
            crate::family::english::EnglishWeights::default(),
            None,
            Box::new(crate::family::magic::expander::DefaultProvider),
            Vec::new(),
            crate::family::scoring::ScoringConfig::default(),
            Arc::new(crate::frontend::NoopFrontend::default()),
            DEFAULT_VOICE_AURA_BASE.to_string(),
            crate::io_thread::DEFAULT_IDLE_TIMEOUT_SECS,
            Vec::new(),
            7,
        )
    }

    /// Create engine with full config (pinyin weights + English weights).
    /// `provider` resolves snippet variables (`$DATE`, `$CLIPBOARD`, …) — inject a
    /// platform provider here; the engine keeps a shared `Arc` so later
    /// [`set_variable`](ImeEngine::set_variable) updates reach the expander.
    ///
    /// `extra_snippets` are user-defined `(trigger, expansion)` pairs merged over
    /// the built-ins — on trigger collision the config entry wins (trie nodes are
    /// overwritten last-writer-wins).
    ///
    /// `scoring` carries every configurable scoring parameter (family priorities,
    /// recency boosts, bigram ceiling, freq→score scale) from `swift-ime.yaml`;
    /// `Default` reproduces the legacy hardcoded values exactly.
    ///
    /// `frontend` 是前端句柄 —— 引擎的单条 I/O 线程经它推送 UI 刷新 / 请求
    /// 剪贴板。前端不再轮询。
    ///
    /// `voice_aura_base` 是 aura daemon origin(`http://127.0.0.1:9091`)。
    /// 引擎构造时立即启动 voice listener task(`#asr` 共享同一份 `AuraClient`),
    /// 整生命周期跟随 engine drop。
    ///
    /// `voice_idle_timeout_secs` 是语音连接空闲自动断连时长(秒,0 = 永不主动断),
    /// 默认 [`DEFAULT_IDLE_TIMEOUT_SECS`](crate::io_thread::DEFAULT_IDLE_TIMEOUT_SECS)。
    #[allow(clippy::too_many_arguments)] // 构造注入点:参数即依赖清单
    pub fn with_config(
        pinyin_weights: crate::family::pinyin::PinyinWeights,
        english_weights: crate::family::english::EnglishWeights,
        // 英文 base 词表路径(hermitdave en_freq.tsv;None = 空 base)。
        english_wordlist: Option<String>,
        provider: Box<dyn crate::family::magic::expander::VariableProvider>,
        extra_snippets: Vec<crate::store::snippet_md::SnippetEntry>,
        scoring: crate::family::scoring::ScoringConfig,
        frontend: Arc<dyn crate::frontend::FrontEndHandle>,
        voice_aura_base: String,
        voice_idle_timeout_secs: u64,
        addons: Vec<crate::family::magic::AddonConfig>,
        page_size: u32,
    ) -> Self {
        // Magic command entries are generated from the member registry (#asr, #flush,
        // #submit, #req …) — adding a command = one member, nothing
        // here. `/`-snippets are now the empty-name snippet magic command (`#/sig`).
        let mut magic = Arc::new(MagicFamily::new());
        // 注册配置化 addon 插件命令 —— 必须在 matcher 构建前(magic 此刻
        // refcount=1,Arc::get_mut 安全)。
        if let Some(m) = Arc::get_mut(&mut magic) {
            m.register_addons(&addons);
        }
        // 片段注册表:内置 + 外部注入(SNIP md / 配置);名字为片段名(如 `sig`,
        // 调用 `#/sig`)。
        let mut snippets: Vec<SnippetEntry> = vec![
            SnippetEntry {
                name: "greet".into(),
                comment: String::new(),
                params: Vec::new(),
                template: "你好，我是 AI 秘书，请问有什么可以帮你的？".into(),
            },
            SnippetEntry {
                name: "sig".into(),
                comment: String::new(),
                params: Vec::new(),
                template: "Best regards,\nAlice".into(),
            },
        ];
        snippets.extend(extra_snippets);
        magic.set_snippets(snippets);
        // Shared with the snippet expander — `set_variable` 经 Expander 写入
        // 同一 Arc(round12:引擎不再另存 provider 字段)。
        let provider: Arc<dyn crate::family::magic::expander::VariableProvider> =
            Arc::from(provider);
        let expander = crate::Expander::new(provider);
        // 共享 voice state(voice server 折叠写入、#asr 成员同步读)。
        let voice_state = Arc::new(crate::family::magic::SharedTranscript::new());
        magic.set_voice_state(Arc::clone(&voice_state));
        // 单条 tokio I/O 线程 = 多事件源 server(通用 rx + voice server)。
        // voice server 按需(#asr Attach)才连 aura,engine drop → io_thread
        // drop → runtime drop,一切自动清理。
        let io_thread = Arc::new(crate::io_thread::IoThread::spawn(
            std::sync::Arc::downgrade(&frontend),
            voice_aura_base,
            Arc::clone(&voice_state),
            voice_idle_timeout_secs,
        ));
        magic.set_io(Arc::clone(&io_thread), Arc::clone(&frontend));
        // `#asr` 家族经同一 sender 发 Attach/Detach 给 voice server。
        magic.set_voice_tx(io_thread.voice_tx());
        // pinyin + english + emoji compete in the unified scorer (中英混输 +
        // emoji). Magic (#) and snippet (/) are routed by the FSM via the
        // matcher — their candidates never pass through the scorer.
        // 单词本(round14):持久化模块所有;引擎/SessionState/两家族持引用。
        let wordbook = Arc::new(crate::store::wordbook::WordBook::default());
        let pinyin_family = Arc::new(crate::family::pinyin::PinyinFamily::with_scoring_and_phrase_book(
            pinyin_weights,
            scoring,
            None,
            Arc::clone(&wordbook),
        ));
        let mut english_family = crate::family::english::EnglishFamily::with_base_wordlist(
            english_wordlist.as_deref(),
        )
        .with_config(scoring.priorities.english, english_weights);
        english_family.set_wordbook(Arc::clone(&wordbook));
        let english_family = Arc::new(english_family);
        let emoji_family = std::sync::Arc::new(crate::family::emoji::EmojiFamily::new());
        let scorer = crate::family::UnifiedScorer::new(
            vec![
                Box::new(Arc::clone(&pinyin_family)),
                Box::new(Arc::clone(&english_family)),
                Box::new(Arc::clone(&emoji_family)),
            ],
            scoring.priorities,
        );
        ImeEngine {
            expander,
            wordbook,
            scorer,
            pinyin_family,
            english_family,
            emoji_family,
            sessions: Mutex::new(HashMap::new()),
            persistence: Mutex::new(None),
            magic,
            page_size: page_size.max(1),
            candidate_meta: false,
            frontend,
            io_thread,
            filters: crate::fsm::post::FilterChain::with_flood_control(scoring.floors),
        }
    }

    /// 前端句柄(引擎 I/O 线程经它推送刷新 / 请求剪贴板)。
    pub fn frontend(&self) -> Arc<dyn crate::frontend::FrontEndHandle> {
        Arc::clone(&self.frontend)
    }

    /// 共享 voice state 句柄。voice listener task 与魔法成员都通过它读 / 写。
    /// 实体存于 MagicFamily 的 VoiceStateSlot(round12:引擎不再另存字段,
    /// 从槽里取 —— 装配期 set_voice_state 必已发生)。
    pub fn voice_state(&self) -> Arc<crate::family::magic::SharedTranscript> {
        self.magic
            .resources()
            .voice_state()
            .expect("voice state is set at engine assembly")
    }

    /// 引擎的单条 tokio I/O 线程句柄。
    pub fn io_thread(&self) -> Arc<crate::io_thread::IoThread> {
        Arc::clone(&self.io_thread)
    }

    // ── ctx helpers ─────────────────────────────────────────────────────

    fn with_ctx<T>(&self, ctx: usize, f: impl FnOnce(&ImeEngine, &mut Session) -> T) -> T {
        // 锁中毒(某次 with_ctx 闭包 panic 过)不传染 —— 恢复内部数据继续,
        // 状态机/pipeline 数据本身未损坏,panic 会把整个引擎打死。
        let mut map = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pc = map
            .entry(ctx)
            .or_insert_with(|| Session::with_page_size(self.page_size, self.candidate_meta, std::sync::Arc::clone(&self.wordbook)));
        pc.pane.session.ctx = ctx; // 装配:ctx 挂号
        f(self, pc)
    }

    /// 调试模式:候选词后显示提供者与权重(swift-ime.yaml → debug.candidate_meta)。
    /// 已存在的 context 立即生效,后续新建的 context 沿用。
    pub fn set_candidate_meta(&mut self, on: bool) {
        self.candidate_meta = on;
        for pc in self.sessions.lock().unwrap().values_mut() {
            pc.set_candidate_meta(on);
        }
    }

    /// 运行时启/禁某家族(`dicts.emoji: false` → "emoji" 禁用,无 emoji 候选)。
    pub fn set_family_enabled(&self, name: &str, on: bool) {
        if let Some(fam) = self.scorer.family(name) {
            fam.set_family_enabled(on);
        }
    }

    /// 配置某家族进入统一排序的候选宽度(round10:`weights.family_top_n`)。
    /// 这是跨家族竞争宽度的语义截断入口;视图槽位(CANDIDATE_SLOTS=48)是
    /// 翻页窗口硬顶,两者独立。0 = 回落家族默认。
    pub fn set_family_top_n(&self, name: &str, n: usize) {
        self.scorer.set_family_top_n(name, n);
    }

    /// 注册 stage3 候选过滤器(round10 W7 框架):postprocess 在合成/置顶
    /// 之后按注册序跑链,Drop 移除、Demote 调分。需 `&mut self` —— 在引擎
    /// 发布为 Arc 前的配置阶段调用(fcitx5 `swift_ime_create` /
    /// TUI `build_engine` 均有该窗口)。
    pub fn add_filter(&mut self, f: Box<dyn crate::fsm::post::CandidateFilter>) {
        self.filters.push(f);
    }

    /// 已注册的过滤器数(调试/日志)。
    pub fn filter_count(&self) -> usize {
        self.filters.len()
    }

    /// L2 OverlayDict 冷加载(round19 三级架构)。
    pub fn warm_overlay_dict(&self, freq: Vec<(String, String, u64, i64, u32)>, recent: Vec<(String, i64)>) {
        if freq.is_empty() && recent.is_empty() {
            return;
        }
        let n = freq.len();
        self.wordbook
            .overlay_dict
            .lock()
            .unwrap()
            .load(freq, recent);
        // L2 → lattice overlay 旁路同步(round19):首查前即生效。
        self.pinyin_family.sync_lattice_overlay();
        eprintln!("[ime-core] overlay_dict: cold-loaded {n} freq entries");
    }

    /// 临时关闭/恢复上下文感知(swift-ime.yaml → input.context_aware)。
    /// 同时作用于两个家族:拼音的 recency/整词联想/bigram,英文的 recency。
    /// 关闭后候选排序纯频率驱动。
    pub fn set_context_aware(&mut self, on: bool) {
        self.pinyin_family.set_context_aware(on);
        self.english_family.set_context_aware(on);
    }

    /// 候选每页条数 —— 运行时动态调整。构造期请传 `with_config` 的
    /// `page_size` 参数(swift-ime.yaml → input.page_size 由 app 层读取后
    /// 注入,ime-core 不读配置文件)。
    /// 已存在的 context 立即生效,后续新建的 context 沿用新值。
    pub fn set_page_size(&mut self, page_size: u32) {
        if page_size == 0 {
            return;
        }
        self.page_size = page_size;
        for pc in self.sessions.lock().unwrap().values_mut() {
            // 面板是会话状态:写入口在 stage2 门面(round12)。
            pc.set_page_size(page_size as usize);
        }
    }

    fn remove_ctx(&self, ctx: usize) {
        // 修复:取走 `Session` 时先调 active_command 的 deactivate(ctx),
        // 让魔法成员释放订阅 / 任务 —— 之前的 `drop` 默认实现直接走,某些
        // live member(如 VoiceMember)需要显式 deactivate 才能取消后台工作。
        let mut map = self.sessions.lock().unwrap();
        if let Some(mut pc) = map.remove(&ctx) {
            // 成员生命周期(释放订阅 / 取消后台任务)内聚在 stage2 门面。
            pc.clear_active_command();
        }
    }

    // ── Multi-context API (used by fcitx5 C ABI) ────────────────────────

    /// **统一事件入口**(round12):前端的一切动作封装为 [`ImeEvent`]
    /// (键盘 / 控制 / 异步三类),从 stage1(系统控制)进,由
    /// [`ControlStage`] 裁决去向。返回 `None` 仅用于异步事件无推进
    /// (前端据此跳过刷新)。现有便捷出口(`key_ctx`/`select_ctx`/…)
    /// 全部是这里的薄包装。
    pub fn event_ctx(&self, ctx: usize, event: ImeEvent) -> Option<ImeView> {
        self.with_ctx(ctx, |disp, pc| {
            // stage1 统一入口:action 归一化 / flags 镜像都在状态机表内收口。
            pc.handle(event, disp)
        })
    }

    /// **统一键入口**:所有前端把键(含特殊键与 Ctrl/Shift/Alt 修饰状态)
    /// 忠实地转成 [`KeyEvent`] 喂到这里。输入路由层(状态机表)查表决定
    /// 这枚键属于输入法还是应用,驱动组合状态机迁移,返回带 action 位
    /// 标志的视图 —— 外界按 [`action`](crate::frontend::action) 反应即可,
    /// 不再自行拦截任何键。
    pub fn key_ctx(&self, ctx: usize, key: KeyEvent) -> ImeView {
        // 提交落地(context 滚动 / recency / 自生词学习)由管线内部
        // commit_text 统一处理 —— engine 壳零回写。
        self.event_ctx(ctx, ImeEvent::Key(key))
            .unwrap_or_else(ImeView::empty)
    }

    /// 当前输入上下文的状态标志位(状态机表)。TUI 状态栏 / 调试用。
    pub fn state_flags_ctx(&self, ctx: usize) -> StateFlags {
        self.sessions
            .lock()
            .unwrap()
            .get(&ctx)
            .map(|pc| pc.flags())
            .unwrap_or_else(StateFlags::empty)
    }

    /// Process a character key for a given input context(旧字符入口的薄包装,
    /// 归一化后走 [`key_ctx`])。
    pub fn predict_ctx(&self, ctx: usize, ch: char) -> ImeView {
        self.key_ctx(ctx, KeyEvent::char(ch))
    }

    /// Select a candidate by index for a given context.
    pub fn select_ctx(&self, ctx: usize, index: usize) -> ImeView {
        self.event_ctx(ctx, ImeEvent::Control(ControlEvent::Select(index)))
            .unwrap_or_else(ImeView::empty)
    }

    /// Reset engine state for a context.
    pub fn reset_ctx(&self, ctx: usize) {
        self.event_ctx(ctx, ImeEvent::Control(ControlEvent::Reset));
    }

    /// Deactivate (clean up) a context — removes its state and async waits.
    pub fn deactivate_ctx(&self, ctx: usize) {
        self.remove_ctx(ctx);
    }

    /// Set surrounding text from the application (fcitx5 callback).
    /// The text is stored in per-context `InputContext` and used by
    /// prediction families for broader context matching.
    /// Commit any pending composition for a context.
    pub fn commit_pending_ctx(&self, ctx: usize) -> ImeView {
        let map = self.sessions.lock().unwrap();
        let Some(pc) = map.get(&ctx) else {
            return ImeView::empty();
        };
        // 提交语义(大小写回填 / raw_buffer 兜底)内聚在 stage2 门面。
        let text = pc.pending_commit_text();
        let mut v = ImeView::empty();
        if !text.is_empty() {
            ImeView::set_str(&mut v.commit_text, &text);
        }
        v
    }

    // ── Single-context convenience API (tests / mock) ───────────────────

    /// Feed a [`KeyEvent`] into the default context (ctx=0) — 单上下文版的
    /// [`key_ctx`](ImeEngine::key_ctx)。
    pub fn key(&mut self, key: KeyEvent) -> ImeView {
        self.key_ctx(DEFAULT_CTX, key)
    }

    /// Feed an KeyEvent(= [`KeyEvent`],旧名)into the default context.
    pub fn predict(&mut self, event: KeyEvent) -> ImeView {
        self.key(event)
    }

    /// 当前(default ctx)状态标志位。
    pub fn state_flags(&self) -> StateFlags {
        self.state_flags_ctx(DEFAULT_CTX)
    }

    /// Select a candidate in the default context.
    pub fn select_candidate(&mut self, index: usize) -> ImeView {
        self.select_ctx(DEFAULT_CTX, index)
    }

    /// Rebuild the ImeView from current state (for display after navigation).
    /// Returns the full UI snapshot without processing a key event.
    pub fn view(&self) -> ImeView {
        self.sessions
            .lock()
            .unwrap()
            .get(&DEFAULT_CTX)
            .map(|pc| pc.snapshot_view())
            .unwrap_or_else(ImeView::empty)
    }

    /// Current pinyin buffer for the default context.
    pub fn buffer(&self) -> String {
        self.sessions
            .lock()
            .unwrap()
            .get(&DEFAULT_CTX)
            .map(|pc| pc.buffer())
            .unwrap_or_default()
    }

    /// Current candidates for the default context.
    pub fn candidates(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap()
            .get(&DEFAULT_CTX)
            .map(|pc| pc.candidates())
            .unwrap_or_default()
    }

    /// Attach weight store to families for persistence(persistence 双写路径)。
    pub(crate) fn set_store(&self, store: Arc<crate::store::WeightStore>) {
        use crate::family::CandidateFamily;
        self.pinyin_family.attach_store(Arc::clone(&store));
        self.english_family.attach_store(store);
    }
    /// Warm the phrase book from persisted SQLite data.
    pub(crate) fn warm_phrases_from_store(&self) {
        self.pinyin_family.warm_phrases_from_store();
    }
    /// L0 user model(pins + pick counters)导入(inputx 引擎词典)。
    pub(crate) fn import_l0(&self, json: &str) -> usize {
        self.pinyin_family.import_l0_json(json)
    }
    /// Warm the english user layer from persisted 英文自生词。
    pub(crate) fn warm_en_user(&self, words: Vec<(String, u32)>) {
        self.english_family.warm_learned_words(&words);
    }
    /// Warm the pinyin family's recency ring from persisted data。
    /// 候选元数据(与 [`candidates`](Self::candidates) 同序)—— 测试断言
    /// meta 对齐用;调试视图经 view.candidates[].meta 走 fill_view。
    #[cfg(test)]
    pub(crate) fn last_meta(&self) -> Vec<crate::fsm::post::CandMeta> {
        self.sessions
            .lock()
            .unwrap()
            .get(&DEFAULT_CTX)
            .map(|pc| pc.last_meta())
            .unwrap_or_default()
    }

    /// Current candidates with full detail (source, score) for debugging.
    /// 面板镜像(同序同源):Snippet 会话分支与 scorer 路径都内聚在
    /// [`ControlPane::detailed`] —— 壳只调门面。
    pub fn candidates_detailed(&self) -> Vec<crate::family::RankedCandidate> {
        let map = self.sessions.lock().unwrap();
        map.get(&DEFAULT_CTX)
            .map(|pc| pc.detailed())
            .unwrap_or_default()
    }

    /// Manually set the text context (simulates pre-filled text).
    pub fn set_context(&mut self, text: &str) {
        self.sessions
            .lock()
            .unwrap()
            .entry(DEFAULT_CTX)
            .or_insert_with(|| Session::with_page_size(self.page_size, self.candidate_meta, std::sync::Arc::clone(&self.wordbook)))
            .pane
            .session
            .context
            .update(text);
    }

    /// Load an external dictionary into the PinyinFamily's phrase book.
    /// Supports TSV (`pinyin\tword`) and JSON (`[{"pinyin":"...","text":"..."}]`).
    /// Returns number of entries loaded.
    pub fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer.load_dict_to("pinyin", path).unwrap_or_else(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "pinyin family not found",
            ))
        })
    }

    /// Initialize the unified persistence manager. Call once at startup —
    /// warms EVERY persisted user model (bigrams, phrases, recency ring, L0)
    /// into the in-memory stores, then families double-write from here on.
    pub fn init_store(&self, path: &str) {
        match PersistenceManager::open_with_wordbook(path, Arc::clone(&self.wordbook)) {
            Ok(pm) => {
                pm.warm_all(self);
                eprintln!(
                    "[swift-ime] weight store: {} phrases, {} en-words from {path}",
                    pm.phrase_count(),
                    pm.en_user_count()
                );
                *self.persistence.lock().unwrap() = Some(pm);
            }
            Err(e) => eprintln!("[swift-ime] weight store open failed: {e}"),
        }
    }

    /// `#req` backend base URL (default `http://127.0.0.1:14555/api`).
    /// `#req/news?query=soccer` → `GET {base}/news?query=soccer`.
    pub fn set_req_base(&self, base: &str) {
        self.magic.set_req_base(base);
    }

    /// scout(omni-scout)HTTP 注入服务地址 —— `#del` 用它注入退格。
    pub fn set_scout_inject_url(&self, url: &str) {
        self.magic.set_scout_inject_url(url);
    }

    /// scout 注入服务地址(默认 `http://127.0.0.1:7878`)。
    pub fn scout_inject_url(&self) -> String {
        self.magic.scout_inject_url()
    }

    /// Inject an HTTP fetcher for `#req` (tests use a fake; the production default
    /// is a reqwest client behind ime-core's `http` feature).
    pub fn set_req_fetcher(&self, fetcher: Arc<dyn ReqFetcher>) {
        self.magic.set_req_fetcher(fetcher);
    }

    /// Update a snippet variable's value at runtime — e.g. the fcitx5 frontend
    /// pushes clipboard changes here (via the C ABI) so `$CLIPBOARD` templates
    /// expand to the current text. Providers that don't support updates ignore it.
    /// 剪贴板值同时累积进 `#clip` 的历史环。
    pub fn set_variable(&self, name: &str, value: &str) {
        self.expander.set_variable(name, value);
        if name == "CLIPBOARD" {
            self.magic.push_clipboard(value);
        }
    }

    /// Poll for changes while a live magic command (`#asr` voice anchor, `#req`
    /// HTTP request, …) is active. If the member's async state advanced, rebuild
    /// the candidate view. Returns the new view, or None if no live command is
    /// active / nothing changed. Frontends call this from their render loop to
    /// update the candidate area without a keypress.
    pub fn magic_tick(&self) -> Option<ImeView> {
        self.magic_tick_ctx(DEFAULT_CTX)
    }

    pub fn magic_tick_ctx(&self, ctx: usize) -> Option<ImeView> {
        self.with_ctx(ctx, |disp, pc| {
            // 排查流式不刷新:每个 drain 是否到这里、state/has_member 是否正常。
            let (state, has_member) = pc.tick_probe();
            tracing::info!(ctx, state, has_member, "magic_tick_ctx");
            // tick 编排(成员驱动 / predictions 回填 / 视图重建)内聚在
            // stage2;壳经异步事件入口驱动,只留观测日志。
            let view = pc.handle(
                ImeEvent::Async(crate::fsm::event::AsyncEvent::MagicTick),
                disp,
            )?;
            let top = if view.candidate_count > 0 {
                ImeView::str_field(&view.candidates[0].text)
            } else {
                ""
            };
            // 排查"只显示半句":top 是截断前的完整候选文本 —— 若 top 是整句而
            // 面板只显示半句,就是前端截断;若 top 本身就半句,则是折叠/识别问题。
            tracing::info!(
                ctx,
                count = view.candidate_count,
                top,
                "magic_tick_ctx → view"
            );
            Some(view)
        })
    }

    /// ctx 上是否还有**活跃的 #asr 会话**。前端(`FcitxFrontend::refresh_ui`)
    /// 同步查它来告诉 voice server"这次刷新会不会被主循环接受";voice server
    /// 据此在失败时放弃(`active_ctx = -1`)。
    ///
    /// 线程安全:`sessions` 由 `Mutex` 保护,主线程写、I/O 线程读,无竞争。
    pub fn is_voice_ctx_alive(&self, ctx: usize) -> bool {
        let map = self.sessions.lock().unwrap();
        // 会话状态判断(asr 成员存活)内聚在 stage2 门面,壳只留观测日志。
        let (alive, detail) = map
            .get(&ctx)
            .map(|pc| pc.asr_probe())
            .unwrap_or((false, "no-context".into()));
        tracing::debug!(ctx, alive, detail, "is_voice_ctx_alive");
        alive
    }

    /// Load an English user dictionary from a TSV file.
    /// All words get max priority (10000).
    pub fn load_en_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer
            .family("english")
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "english family not found")
            })
            .and_then(|f| f.load_user_dict(path))
    }

    /// yaml `weights.emoji` → emoji 家族打分参数。
    pub fn set_emoji_weights(&self, w: crate::family::emoji::EmojiWeights) {
        self.emoji_family.set_weights(w);
    }

    /// Load the emoji keyword table (v2: `emoji freq kw...`, whitespace-
    /// separated; see family/emoji.rs).
    pub fn load_emoji_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer.load_dict_to("emoji", path).unwrap_or_else(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "emoji family not found",
            ))
        })
    }

    /// Load the user emoji mapping (`emoji_user.tsv`) — overrides everything
    /// loaded before for the same keyword.
    pub fn load_emoji_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer
            .load_user_dict_to("emoji", path)
            .unwrap_or_else(|| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "emoji family not found",
                ))
            })
    }

    /// Load an external English dictionary (auto-detect type, normalize, cache).
    pub fn load_en_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer
            .family("english")
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "english family not found")
            })
            .and_then(|f| f.load_dict(path))
    }
}

impl Default for ImeEngine {
    fn default() -> Self {
        ImeEngine::new()
    }
}

// ── StepEnv:状态机访问家族能力的接面(原 Dispatcher 职责,转发层裁撤)──

impl crate::family::FamilyEnv for ImeEngine {
    fn expander(&self) -> &crate::Expander {
        &self.expander
    }
    fn record_pick(&self, pinyin: &str, word: &str) {
        // 家族私有方法(D5):经具体句柄直调 —— 学习语义只有 pinyin 有。
        self.pinyin_family.record_pick(pinyin, word);
    }
    fn adjust_word_freq(&self, pinyin: &str, word: &str, step: i64) -> Option<(u64, u64)> {
        // round22 #freq/up|down:种子频率补继承 + 单词本账本记账。
        let seed = self.pinyin_family.seed_frequency(pinyin, word);
        self.wordbook.adjust_freq(pinyin, word, step, seed)
    }
    fn compose_single_chars(
        &self,
        input: &str,
        ctx: &crate::family::InputContext,
        existing: &[String],
        limit: usize,
    ) -> Vec<crate::family::ScoredCandidate> {
        self.pinyin_family
            .compose_single_chars(input, ctx, existing, limit)
    }
    fn learn_phrase(&self, pinyin: &str, hanzi: &str) {
        self.pinyin_family.learn_phrase(pinyin, hanzi);
    }
    fn learn_composed_phrase(&self, pinyin: &str, hanzi: &str) {
        self.pinyin_family.learn_composed_phrase(pinyin, hanzi);
    }
    fn record_commit_text(&self, word: &str, family: Option<&str>) {
        if family == Some("english") {
            self.english_family.record_commit(word);
        } else {
            self.pinyin_family.record_commit(word);
        }
    }
    fn record_commit_len(&self, word: &str) {
        *self.magic.resources().last_commit_len.lock().unwrap() = word.chars().count() as u32;
    }
    fn learn_ascii_word(&self, word: &str) {
        // 提交文本是纯 ASCII 字母数字(如 cd)时,学入英文家族 user 层
        // (英文自生词)。汉字/emoji/符号不触发。Enter 强选 raw 的主路径。
        if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric()) {
            self.english_family.record_learned_word(word);
        }
    }
    fn voice_cmd_tx(&self) -> Option<crate::io_thread::VoiceCmdSender> {
        self.magic.voice_cmd_tx()
    }
}

impl crate::fsm::family_prediction::StepEnv for ImeEngine {
    fn scorer(&self) -> &crate::family::UnifiedScorer {
        &self.scorer
    }
    fn filters(&self) -> &crate::fsm::post::FilterChain {
        &self.filters
    }
    fn magic(&self) -> &MagicFamily {
        &self.magic
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 英文词表就绪的引擎(内嵌词表外置后,测试用临时文件提供小词表)。
    /// 文件名含线程 id 且只写一次 —— 并行测试共享同路径会互相截断读取。
    fn eng() -> ImeEngine {
        let tid = format!("{:?}", std::thread::current().id());
        let path = std::env::temp_dir()
            .join(format!("eng_words_{}_{tid}.tsv", std::process::id()))
            .to_string_lossy()
            .into_owned();
        if !std::path::Path::new(&path).exists() {
            let words = "world\t50000\nhello\t40000\nthe\t30000\na\t20000\ni\t10000\n\
                         test\t9000\napple\t5000\nblack\t4000\nok\t3000\ncase\t2000\n\
                         english\t8000\n";
            std::fs::write(&path, words).unwrap();
        }
        ImeEngine::with_config(
            crate::family::pinyin::PinyinWeights::default(),
            crate::family::english::EnglishWeights::default(),
            Some(path.clone()),
            Box::new(crate::family::magic::expander::DefaultProvider),
            Vec::new(),
            crate::family::scoring::ScoringConfig::default(),
            Arc::new(crate::frontend::NoopFrontend::default()),
            DEFAULT_VOICE_AURA_BASE.to_string(),
            crate::io_thread::DEFAULT_IDLE_TIMEOUT_SECS,
            Vec::new(),
            7,
        )
    }

    #[test]
    fn meta_aligns_with_candidates_after_compose_rerank() {
        // S2:Layer 3 造词重排后,last_meta 与 candidates 必须同序同源 ——
        // 曾在重排前采样,单字区的 meta 错位显示别人的来源。
        use crate::fsm::control::{KeyEvent, KeyKind};
        let mut e = ImeEngine::new();
        for c in "nihao".chars() {
            e.predict(KeyEvent {
                kind: KeyKind::Char(c),
                ctrl: false,
                shift: false,
                alt: false,
            });
        }
        let cands = e.candidates();
        let meta = e.last_meta();
        assert_eq!(cands.len(), meta.len(), "meta/candidates 同长");
        for (i, m) in meta.iter().enumerate() {
            assert_eq!(m.text, cands[i], "同序: meta[{}] == candidates[{}]", i, i);
        }
        // 单字区(partial)的 meta 是自己的来源(single),不再是别人的。
        if let Some(pos) = cands.iter().position(|c| c == "你") {
            assert_eq!(
                meta[pos].source, "single",
                "单字区 meta 自源: {}",
                meta[pos].source
            );
        }
    }

    #[test]
    fn page_size_flows_from_constructor_to_view_window() {
        // 构造参数 page_size(swift-ime.yaml → input.page_size,app 层读取
        // 后注入)决定翻页窗口滑动步长:页 2 首条 = merged[2×5]。
        use crate::fsm::control::{KeyEvent, KeyKind};
        let mut e = ImeEngine::new();
        e.set_page_size(5);
        e.set_page_size(5);
        for c in "nihao".chars() {
            e.predict(KeyEvent {
                kind: KeyKind::Char(c),
                ctrl: false,
                shift: false,
                alt: false,
            });
        }
        let all = e.candidates();
        e.predict(KeyEvent {
            kind: KeyKind::PageDown,
            ctrl: false,
            shift: false,
            alt: false,
        });
        e.predict(KeyEvent {
            kind: KeyKind::PageDown,
            ctrl: false,
            shift: false,
            alt: false,
        });
        let v = e.predict(KeyEvent {
            kind: KeyKind::PageDown,
            ctrl: false,
            shift: false,
            alt: false,
        });
        assert_eq!(v.candidate_page, 3);
        let head = ImeView::str_field(&v.candidates[0].text);
        assert_eq!(
            Some(head),
            all.get(3 * 5).map(String::as_str),
            "窗口按页大小 5 滑动: 页 3 首 = merged[15]"
        );
    }

    #[test]
    fn candidate_view_pages_slide_over_merged() {
        // 翻页窗口:fill_view 装载"从当前页首起的 16 条"而非固定前 16 ——
        // 造词单字区全量放出后,merged 超过 16 的候选翻页可达。
        // nihao(嵌入词典):merged = [你好] + 单字区 + 链尾,页大小 7。
        use crate::fsm::control::{KeyEvent, KeyKind};
        let mut e = ImeEngine::new();
        for c in "nihao".chars() {
            e.predict(KeyEvent {
                kind: KeyKind::Char(c),
                ctrl: false,
                shift: false,
                alt: false,
            });
        }
        let all = e.candidates();
        assert!(all.len() > 16, "merged 超过 16 槽: {}", all.len());
        // 第 3 页(page 2)首条 = merged[14]。
        for _ in 0..2 {
            e.predict(KeyEvent {
                kind: KeyKind::PageDown,
                ctrl: false,
                shift: false,
                alt: false,
            });
        }
        let v = e.predict(KeyEvent {
            kind: KeyKind::PageDown,
            ctrl: false,
            shift: false,
            alt: false,
        });
        assert_eq!(v.candidate_page, 3);
        let page3_head = ImeView::str_field(&v.candidates[0].text);
        assert_eq!(
            Some(page3_head),
            all.get(3 * 7).map(String::as_str),
            "窗口滑动到页 3: view[0] == merged[21]"
        );
        // 选词全局序:页内第一个候选的提交 = merged[21](partial 单字 →
        // 部分提交;这里只验证窗口内容对齐,不触发提交)。
    }

    #[test]
    fn compose_head_falls_back_when_no_real_words() {
        // 嵌入词典(无 FST)下 nihao 候选全是 decomp 链 —— 造词 head 的
        // 真词过滤必须保底收首候选,否则单字区顶到槽 1,space 变成单字
        // 部分提交(commit_text 为空)。
        use crate::fsm::control::{KeyEvent, KeyKind};
        let mut e = ImeEngine::new();
        for c in "nihao".chars() {
            e.predict(KeyEvent {
                kind: KeyKind::Char(c),
                ctrl: false,
                shift: false,
                alt: false,
            });
        }
        let cands = e.candidates();
        assert_eq!(
            cands.first().map(String::as_str),
            Some("你好"),
            "head 保底: {:?}",
            &cands[..4.min(cands.len())]
        );
        assert!(cands.iter().any(|c| c == "你"), "单字区仍在(head 之后)");
        let v = e.predict(KeyEvent {
            kind: KeyKind::Space,
            ctrl: false,
            shift: false,
            alt: false,
        });
        assert_eq!(ImeView::str_field(&v.commit_text), "你好");
    }

    #[test]
    fn type_pinyin_and_commit() {
        let mut e = eng();
        for c in "nihao".chars() {
            e.predict(KeyEvent::char(c));
        }
        assert!(e.candidates().iter().any(|c| c.contains("你好")));
        let v = e.predict(KeyEvent::space());
        assert!(ImeView::str_field(&v.commit_text).contains("你"));
    }

    #[test]
    fn uppercase_english_predicts_as_lowercase_and_commits_cased() {
        // 大写 E 视作小写 e 预测(词典候选 english 出现),提交保留 English。
        let mut e = eng();
        for c in "English".chars() {
            e.predict(KeyEvent::char(c));
        }
        let cands = e.candidates();
        assert!(
            cands.iter().any(|c| c == "english"),
            "uppercase should predict as lowercase: {cands:?}"
        );
        // preedit 保留原始大小写。
        assert_eq!(ImeView::str_field(&e.view().preedit_text), "English");
        // 选 english 候选 → 提交 English。
        let idx = cands.iter().position(|c| c == "english").unwrap();
        let v = e.select_candidate(idx);
        assert_eq!(ImeView::str_field(&v.commit_text), "English");
    }

    #[test]
    fn uppercase_english_enter_commits_raw_cased() {
        // Enter 强选 raw 文本:提交原始大小写,非小写 buffer。
        let mut e = eng();
        for c in "English".chars() {
            e.predict(KeyEvent::char(c));
        }
        let v = e.predict(KeyEvent::enter());
        assert_eq!(ImeView::str_field(&v.commit_text), "English");
    }

    #[test]
    fn prefix_case_applied_to_completion() {
        // "Engli" → 补全 english:前缀回填大小写,补全段( sh )保持小写。
        let mut e = eng();
        for c in "Engli".chars() {
            e.predict(KeyEvent::char(c));
        }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c == "english"), "{cands:?}");
        let idx = cands.iter().position(|c| c == "english").unwrap();
        let v = e.select_candidate(idx);
        assert_eq!(ImeView::str_field(&v.commit_text), "English");
    }

    #[test]
    fn all_caps_english_commits_all_caps() {
        let mut e = eng();
        for c in "ENGLISH".chars() {
            e.predict(KeyEvent::char(c));
        }
        let cands = e.candidates();
        let idx = cands
            .iter()
            .position(|c| c == "english")
            .expect("english candidate");
        let v = e.select_candidate(idx);
        assert_eq!(ImeView::str_field(&v.commit_text), "ENGLISH");
    }

    #[test]
    fn lowercase_english_unchanged() {
        let mut e = eng();
        for c in "english".chars() {
            e.predict(KeyEvent::char(c));
        }
        let cands = e.candidates();
        let idx = cands
            .iter()
            .position(|c| c == "english")
            .expect("english candidate");
        let v = e.select_candidate(idx);
        assert_eq!(ImeView::str_field(&v.commit_text), "english");
    }

    /// 提交后的候选来源助手:重打一遍,查该词现在的来源。
    fn source_of(e: &mut ImeEngine, word: &str) -> &'static str {
        for c in word.chars() {
            e.predict(KeyEvent::char(c));
        }
        e.candidates_detailed()
            .into_iter()
            .find(|d| d.text == word)
            .map(|d| d.source)
            .unwrap_or("")
    }

    #[test]
    fn committing_english_dict_word_does_not_learn_it_as_user() {
        // 陈年 bug:空格/数字提交英文词典候选 → 词变成 english/user(不该)。
        // 提交来源是英文候选时,不学成自生词。
        let mut e = eng();
        for c in "world".chars() {
            e.predict(KeyEvent::char(c));
        }
        assert_eq!(
            e.candidates_detailed()
                .iter()
                .find(|d| d.text == "world")
                .unwrap()
                .source,
            "exact",
            "world is a dict word",
        );

        e.predict(KeyEvent::space()); // 空格提交高亮(english/exact)→ 缓冲 reset

        // 重新输入:仍是 dict 词,未学成 user。
        for c in "world".chars() {
            e.predict(KeyEvent::char(c));
        }
        assert_eq!(
            e.candidates_detailed()
                .iter()
                .find(|d| d.text == "world")
                .unwrap()
                .source,
            "exact",
            "space-commit must not learn a dict word",
        );

        // 数字选中英文候选同样不学。
        let mut e2 = eng();
        for c in "world".chars() {
            e2.predict(KeyEvent::char(c));
        }
        let idx = e2.candidates().iter().position(|c| c == "world").unwrap();
        e2.select_candidate(idx);
        for c in "world".chars() {
            e2.predict(KeyEvent::char(c));
        }
        assert_eq!(
            e2.candidates_detailed()
                .iter()
                .find(|d| d.text == "world")
                .unwrap()
                .source,
            "exact",
            "digit-select must not learn a dict word",
        );
    }

    #[test]
    fn enter_raw_commit_still_learns_english_word() {
        // Enter 强选 raw(自生词手势)仍学入 user 层。
        let mut e = eng();
        for c in "cd".chars() {
            e.predict(KeyEvent::char(c));
        }
        e.predict(KeyEvent::enter()); // raw commit "cd"
        assert_eq!(source_of(&mut e, "cd"), "user", "raw Enter learns the word");
    }

    #[test]
    fn families_word_books_stay_closed_loop() {
        // 两个家族的单词本各自闭环:
        // - 中文自生词(逐字选)→ 拼音单词本(重新输入出 pinyin/phrase);
        // - 英文 raw Enter → 英文单词本(english/user);
        // - 互不污染:中文不产生英文 user,英文不产生拼音 phrase。

        // 中文自生词:lizhengming 逐字选 → 拼音单词本。
        let mut e = eng();
        for c in "lizhengming".chars() {
            e.predict(KeyEvent::char(c));
        }
        let li = e.candidates().iter().position(|c| c == "李").unwrap();
        e.select_candidate(li);
        let zheng = e.candidates().iter().position(|c| c == "正").unwrap();
        e.select_candidate(zheng);
        let ming = e.candidates().iter().position(|c| c == "明").unwrap();
        e.select_candidate(ming);

        for c in "lizhengming".chars() {
            e.predict(KeyEvent::char(c));
        }
        let detailed = e.candidates_detailed();
        let phrase = detailed
            .iter()
            .find(|d| d.text == "李正明")
            .unwrap_or_else(|| panic!("中文自生词入拼音单词本: {detailed:?}"));
        assert_eq!(phrase.family, "pinyin", "进的是拼音家族单词本");

        // 英文 raw Enter → 英文单词本;family 是 english,不是 pinyin phrase。
        let mut e2 = eng();
        for c in "cd".chars() {
            e2.predict(KeyEvent::char(c));
        }
        e2.predict(KeyEvent::enter());
        for c in "cd".chars() {
            e2.predict(KeyEvent::char(c));
        }
        let detailed = e2.candidates_detailed();
        let cd = detailed
            .iter()
            .find(|d| d.text == "cd")
            .unwrap_or_else(|| panic!("英文 Enter 入英文单词本: {detailed:?}"));
        assert_eq!(cd.family, "english", "进的是英文家族单词本");
        assert_eq!(cd.source, "user");
    }

    #[test]
    fn incremental_composition() {
        let mut e = eng();
        for c in "lizhengming".chars() {
            e.predict(KeyEvent::char(c));
        }
        let li = e.candidates().iter().position(|c| c == "李").unwrap();
        e.select_candidate(li);
        let zheng = e.candidates().iter().position(|c| c == "正").unwrap();
        e.select_candidate(zheng);
        let ming = e.candidates().iter().position(|c| c == "明").unwrap();
        let v = e.select_candidate(ming);
        assert_eq!(ImeView::str_field(&v.commit_text), "李正明");
    }

    #[test]
    fn snippet_query_params_inject_template_variables() {
        // #/hello?name=Mike → 查询参数注入模板变量 $name。
        use crate::family::magic::expander::VariableProvider;
        #[derive(Clone)]
        struct NoVars;
        impl VariableProvider for NoVars {
            fn resolve(&self, _name: &str) -> Option<String> {
                None
            }
        }
        let e = ImeEngine::with_config(
            crate::family::pinyin::PinyinWeights::default(),
            crate::family::english::EnglishWeights::default(),
            None,
            Box::new(NoVars),
            vec![SnippetEntry {
                name: "hello".into(),
                comment: String::new(),
                params: Vec::new(),
                template: "Hello, my name is $name.".into(),
            }],
            crate::family::scoring::ScoringConfig::default(),
            Arc::new(crate::frontend::NoopFrontend::default()),
            DEFAULT_VOICE_AURA_BASE.to_string(),
            crate::io_thread::DEFAULT_IDLE_TIMEOUT_SECS,
            Vec::new(),
            7,
        );
        let mut e = e;
        for c in "#/hello?name=Mike".chars() {
            e.predict(KeyEvent::char(c));
        }
        let v = e.predict(KeyEvent::space());
        assert_eq!(
            ImeView::str_field(&v.commit_text),
            "Hello, my name is Mike."
        );
    }

    #[test]
    fn snippet_unknown_name_shows_hint_and_commits_empty() {
        // 未知片段名 → 候选"未知片段 /nope",Space 空提交。
        let mut e = eng();
        for c in "#/nope".chars() {
            e.predict(KeyEvent::char(c));
        }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("未知片段")), "{cands:?}");
        let v = e.predict(KeyEvent::space());
        assert!(
            ImeView::str_field(&v.commit_text).is_empty(),
            "unknown snippet commits nothing"
        );
    }

    #[test]
    fn backspace_clears() {
        let mut e = eng();
        e.predict(KeyEvent::char('n'));
        e.predict(KeyEvent::char('i'));
        assert_eq!(e.buffer(), "ni");
        e.predict(KeyEvent::backspace());
        assert_eq!(e.buffer(), "n");
        e.predict(KeyEvent::backspace());
        assert!(e.buffer().is_empty());
    }

    #[test]
    fn multi_context_isolation() {
        let e = eng();
        // Type "ni" in context A
        e.predict_ctx(1, 'n');
        e.predict_ctx(1, 'i');
        // Type "ha" in context B
        e.predict_ctx(2, 'h');
        e.predict_ctx(2, 'a');
        // Deactivate B
        e.deactivate_ctx(2);
        // A should still have "ni"
        let view = e.predict_ctx(1, ' ');
        assert!(ImeView::str_field(&view.commit_text).contains("你"));
    }
}

/// 引擎关闭保底(round19):L1 工作集未达 flush 阈值也不丢 ——
/// 无条件搬入 L2 并落盘(数据已搬走才允许连接关闭)。
impl Drop for ImeEngine {
    fn drop(&mut self) {
        let n = self.wordbook.flush_now();
        if n > 0 {
            tracing::info!(n, "engine drop: flushed overlay workset into L2");
        }
    }
}
