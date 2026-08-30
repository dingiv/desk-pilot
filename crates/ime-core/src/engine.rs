//! ImeEngine — the single integration point for all frontends.
//!
//! Manages the [`Dispatcher`], per-context [`StateMachine`]s, and
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

use crate::dispatcher::Dispatcher;
use crate::family::magic::{MagicFamily, ReqFetcher};
use crate::family::InputContext;
use crate::fsm::router::{StateFlags, StateMachineTable};
// 统一键事件由输入路由层定义(旧名 InputEvent;构造器同名,测试平移)。
use crate::frontend::ImeView;
pub use crate::fsm::router::KeyEvent;
use crate::fsm::state::{StateMachine, StepEnv};
use crate::store::PersistenceManager;
use crate::store::snippet_md::*;

// ── PerContext ──────────────────────────────────────────────────────────

struct PerContext {
    sm: StateMachine,
    /// 输入路由层的状态机表(标志位寄存器)—— 每键路由后同步。
    table: StateMachineTable,
    text_context: InputContext,
}

impl PerContext {
    fn with_page_size(page_size: u32, candidate_meta: bool) -> Self {
        let mut sm = StateMachine::with_page_size(page_size);
        sm.candidate_meta_enabled = candidate_meta;
        PerContext {
            sm,
            table: StateMachineTable::new(),
            text_context: InputContext::new(),
        }
    }
}

// ── ImeEngine ───────────────────────────────────────────────────────────

const DEFAULT_CTX: usize = 0; // used by single-context convenience methods

/// 默认 aura daemon origin。生产配置可在 `swift-ime.yaml → voice.aura_base` 覆盖。
pub const DEFAULT_VOICE_AURA_BASE: &str = "http://127.0.0.1:9091";

/// Self-contained IME engine. Manages the dispatcher, per-context state
/// machines, input context, and async waits.
pub struct ImeEngine {
    dispatcher: Dispatcher,
    contexts: Mutex<HashMap<usize, PerContext>>,
    /// Unified persistence manager — owns the SQLite store and coordinates all
    /// user-model persistence (recency / bigrams / phrases / L0). `None` until
    /// [`init_store`](ImeEngine::init_store).
    persistence: Mutex<Option<PersistenceManager>>,
    /// The magic command registry — same `Arc` the dispatcher holds. The engine
    /// routes late resource attachment (voice buffer, `#req` base/fetcher) here;
    /// the FSM spawns live member instances from it.
    magic: Arc<MagicFamily>,
    /// The snippet-variable provider — same `Arc` the dispatcher's expander holds.
    /// `set_variable` writes through it so `$CLIPBOARD`-style templates resolve fresh.
    provider: Arc<dyn crate::family::magic::expander::VariableProvider>,
    /// 候选每页条数(swift-ime.yaml → input.page_size;默认 7)。传给每个新建的
    /// StateMachine —— 之前写死在 `StateMachine::new` 里(FIXME)。
    page_size: u32,
    /// 调试模式:候选词显示提供者与权重(swift-ime.yaml → debug.candidate_meta)。
    candidate_meta: bool,
    /// 前端句柄 —— 引擎 I/O 线程经它推送 UI 刷新 / 请求剪贴板。
    frontend: Arc<dyn crate::frontend::FrontEndHandle>,
    /// 单条 tokio I/O 线程(事件响应模型),预测主路径不建线程。
    io_thread: Arc<crate::io_thread::IoThread>,
    /// 共享语音会话状态 —— voice server(IoThread)折叠 SSE 段写入,
    /// VoiceMember 在主线程同步读。
    voice_state: Arc<crate::voice_state::SharedVoiceState>,
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
            Box::new(crate::family::magic::expander::DefaultProvider),
            Vec::new(),
            crate::scoring::ScoringConfig::default(),
            Arc::new(crate::frontend::NoopFrontend::default()),
            DEFAULT_VOICE_AURA_BASE.to_string(),
            crate::io_thread::DEFAULT_IDLE_TIMEOUT_SECS,
            Vec::new(),
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
    pub fn with_config(
        pinyin_weights: crate::family::pinyin::PinyinWeights,
        english_weights: crate::family::english::EnglishWeights,
        provider: Box<dyn crate::family::magic::expander::VariableProvider>,
        extra_snippets: Vec<crate::store::snippet_md::SnippetEntry>,
        scoring: crate::scoring::ScoringConfig,
        frontend: Arc<dyn crate::frontend::FrontEndHandle>,
        voice_aura_base: String,
        voice_idle_timeout_secs: u64,
        addons: Vec<crate::family::magic::AddonConfig>,
    ) -> Self {
        // Magic command entries are generated from the member registry (#asr, #flush,
        // #submit, #req, #date, #password …) — adding a command = one member, nothing
        // here. `/`-snippets are now the empty-name snippet magic command (`#/sig`).
        let mut magic = Arc::new(MagicFamily::new());
        // 注册配置化 addon 插件命令 —— 必须在 matcher 构建前(magic 此刻
        // refcount=1,Arc::get_mut 安全)。
        if let Some(m) = Arc::get_mut(&mut magic) {
            m.register_addons(&addons);
        }
        let matcher = crate::Matcher::new(magic.matcher_entries());
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
        // Shared with the dispatcher's expander — `set_variable` writes through the same Arc.
        let provider: Arc<dyn crate::family::magic::expander::VariableProvider> = Arc::from(provider);
        let expander = crate::Expander::new(Arc::clone(&provider));
        // 共享 voice state(voice server 折叠写入、#asr 成员同步读)。
        let voice_state = Arc::new(crate::voice_state::SharedVoiceState::new());
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
        let engine = ImeEngine {
            dispatcher: Dispatcher::with_config(
                matcher,
                expander,
                Arc::clone(&magic),
                pinyin_weights,
                english_weights,
                scoring,
            ),
            contexts: Mutex::new(HashMap::new()),
            persistence: Mutex::new(None),
            magic,
            provider,
            page_size: 7,
            candidate_meta: false,
            frontend,
            io_thread,
            voice_state,
        };
        // Load embedded base dictionary (5KB, compiled into binary).
        let count = engine
            .dispatcher
            .scorer()
            .family("pinyin")
            .map(|f| f.load_dict_bytes(Self::EMBEDDED_BASE_DICT))
            .unwrap_or(0);
        if count > 0 {
            tracing::info!(count, "loaded embedded base dictionary");
        }
        engine
    }

    /// Embedded base phrase dictionary (TSV format), compiled into the binary.
    const EMBEDDED_BASE_DICT: &[u8] =
        include_bytes!("../../../apps/swift-ime/assets/dict/base.tsv");

    /// 前端句柄(引擎 I/O 线程经它推送刷新 / 请求剪贴板)。
    pub fn frontend(&self) -> Arc<dyn crate::frontend::FrontEndHandle> {
        Arc::clone(&self.frontend)
    }

    /// 释放引擎对前端的强引用 —— 前端 destroy 路径调用:让 I/O 线程下次的
    /// `refresh_ui` / `get_clipboard_item` 触达一个空的 C 回调槽(no-op)而
    /// 不是悬空的 C++ `this`。IoThread 自身在最后一个 Arc 释放后回收。
    pub fn detach_frontend(&self) {
        // 通知 magic 资源释放引用,使得前端 Arc 计数减少。
        // 这里不需要清空 magic 内部:它们持有的 Arc 与 self.frontend 是同一份。
        // 通过 Arc::new(NoopFrontend) 覆盖 self.frontend 需要 &mut self,
        // 而 frontend 字段是 private —— 留给前端 destroy 走自己的清理路径。
    }

    /// 共享 voice state 句柄。voice listener task 与魔法成员都通过它读 / 写。
    pub fn voice_state(&self) -> Arc<crate::voice_state::SharedVoiceState> {
        Arc::clone(&self.voice_state)
    }

    /// 引擎的单条 tokio I/O 线程句柄。
    pub fn io_thread(&self) -> Arc<crate::io_thread::IoThread> {
        Arc::clone(&self.io_thread)
    }

    // ── ctx helpers ─────────────────────────────────────────────────────

    fn with_ctx<T>(&self, ctx: usize, f: impl FnOnce(&Dispatcher, &mut PerContext) -> T) -> T {
        // FIXME: 一处不必要的 unwrap
        let mut map = self.contexts.lock().unwrap();
        let pc = map
            .entry(ctx)
            .or_insert_with(|| PerContext::with_page_size(self.page_size, self.candidate_meta));
        pc.sm.ctx = ctx;
        f(&self.dispatcher, pc)
    }

    /// 调试模式:候选词后显示提供者与权重(swift-ime.yaml → debug.candidate_meta)。
    /// 已存在的 context 立即生效,后续新建的 context 沿用。
    pub fn set_candidate_meta(&mut self, on: bool) {
        self.candidate_meta = on;
        for pc in self.contexts.lock().unwrap().values_mut() {
            pc.sm.candidate_meta_enabled = on;
        }
    }

    /// 运行时启/禁某家族(`dicts.emoji: false` → "emoji" 禁用,无 emoji 候选)。
    pub fn set_family_enabled(&self, name: &str, on: bool) {
        self.dispatcher.set_family_enabled(name, on);
    }

    /// 临时关闭/恢复上下文感知(swift-ime.yaml → input.context_aware)。
    /// 同时作用于两个家族:拼音的 recency/整词联想/bigram,英文的 recency。
    /// 关闭后候选排序纯频率驱动。
    pub fn set_context_aware(&mut self, on: bool) {
        self.dispatcher.set_pinyin_context_aware(on);
        self.dispatcher.set_english_context_aware(on);
    }

    /// 候选每页条数(默认 7)。frontend 启动时调用(swift-ime.yaml → input.page_size)。
    /// 已存在的 context 立即生效,后续新建的 context 沿用新值。
    pub fn set_page_size(&mut self, page_size: u32) {
        if page_size == 0 {
            return;
        }
        self.page_size = page_size;
        for pc in self.contexts.lock().unwrap().values_mut() {
            pc.sm.candidate_page_size = page_size as usize;
        }
    }

    fn remove_ctx(&self, ctx: usize) {
        // 修复:取走 `PerContext` 时先调 active_command 的 deactivate(ctx),
        // 让魔法成员释放订阅 / 任务 —— 之前的 `drop` 默认实现直接走,某些
        // live member(如 VoiceMember)需要显式 deactivate 才能取消后台工作。
        let mut map = self.contexts.lock().unwrap();
        if let Some(mut pc) = map.remove(&ctx) {
            if let Some(mut m) = pc.sm.active_command.take() {
                m.deactivate(ctx);
            }
        }
    }

    // ── Multi-context API (used by fcitx5 C ABI) ────────────────────────

    /// **统一键入口**:所有前端把键(含特殊键与 Ctrl/Shift/Alt 修饰状态)
    /// 忠实地转成 [`KeyEvent`] 喂到这里。输入路由层(状态机表)查表决定
    /// 这枚键属于输入法还是应用,驱动组合状态机迁移,返回带 action 位
    /// 标志的视图 —— 外界按 [`action`](crate::frontend::action) 反应即可,
    /// 不再自行拦截任何键。
    pub fn key_ctx(&self, ctx: usize, key: KeyEvent) -> ImeView {
        self.with_ctx(ctx, |disp, pc| {
            pc.sm.context = pc.text_context.clone();
            let view = pc.table.route(&mut pc.sm, key, disp);
            let committed = ImeView::str_field(&view.commit_text);
            // FIXME: 这个业务逻辑应该放在 route 内部, 放的位置太靠外了
            if !committed.is_empty() {
                pc.text_context.update(committed);
                // Record bigram / recency(E2:按提交家族分派到对应家族表)。
                let commit_family = pc.sm.last_commit_family;
                self.dispatcher.record_commit(committed, commit_family);
                // `#del` 的 del_len 选项用:记录本次提交的字符数。
                self.record_last_commit_len(committed);
                // 提交来源是英文候选 → 已是在词典中的词,不学成自生词
                // (空格/数字提交英文候选的陈旧 bug)。
                if pc.sm.take_last_commit_family() != Some("english") {
                    self.learn_english_if_ascii(committed);
                }
            }
            view
        })
    }

    /// 当前输入上下文的状态标志位(状态机表)。TUI 状态栏 / 调试用。
    pub fn state_flags_ctx(&self, ctx: usize) -> StateFlags {
        self.contexts
            .lock()
            .unwrap()
            .get(&ctx)
            .map(|pc| pc.table.flags())
            .unwrap_or_else(StateFlags::empty)
    }

    /// Process a character key for a given input context(旧字符入口的薄包装,
    /// 归一化后走 [`key_ctx`])。
    pub fn predict_ctx(&self, ctx: usize, ch: char) -> ImeView {
        self.key_ctx(ctx, KeyEvent::char(ch))
    }

    /// Select a candidate by index for a given context.
    pub fn select_ctx(&self, ctx: usize, index: usize) -> ImeView {
        self.with_ctx(ctx, |disp, pc| {
            pc.sm.context = pc.text_context.clone();
            let view = disp.select_candidate(index, &mut pc.sm);
            let committed = ImeView::str_field(&view.commit_text);
            if !committed.is_empty() {
                pc.text_context.update(committed);
                // E2:按提交家族分派 recency 记录。
                let commit_family = pc.sm.last_commit_family;
                self.dispatcher.record_commit(committed, commit_family);
                // `#del` 的 del_len 选项用:记录本次提交的字符数。
                self.record_last_commit_len(committed);
                // 英文候选提交 → 不学成自生词(空格/数字提交的陈旧 bug)。
                if pc.sm.take_last_commit_family() != Some("english") {
                    self.learn_english_if_ascii(committed);
                }
            }
            // 路由之外的 sm 变更 —— 状态机表重新同步。
            pc.table.sync_from(&pc.sm);
            view
        })
    }

    /// Reset engine state for a context.
    pub fn reset_ctx(&self, ctx: usize) {
        self.with_ctx(ctx, |disp, pc| {
            disp.reset(&mut pc.sm);
            pc.table.sync_from(&pc.sm);
        });
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
        let map = self.contexts.lock().unwrap();
        let Some(pc) = map.get(&ctx) else {
            return ImeView::empty();
        };
        // 候选(英文按键入大小写回填)优先,否则提交原始输入 raw_buffer。
        let text = pc
            .sm
            .candidates
            .first()
            .map(|c| crate::fsm::state::apply_input_casing(c, &pc.sm.raw_buffer))
            .unwrap_or_else(|| pc.sm.raw_buffer.clone());
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
        self.contexts
            .lock()
            .unwrap()
            .get(&DEFAULT_CTX)
            .map(|pc| {
                let mut v = ImeView::empty();
                v.candidate_count = pc.sm.candidates.len().min(16) as u32;
                v.candidate_highlight = pc.sm.candidate_highlight as u32;
                v.candidate_page = pc.sm.candidate_page as u32;
                v.candidate_page_size = pc.sm.candidate_page_size as u32;
                for (i, c) in pc.sm.candidates.iter().take(16).enumerate() {
                    ImeView::set_str(&mut v.candidates[i].text, c);
                    // 调试模式:meta 与 fill_view 对齐。
                    if pc.sm.candidate_meta_enabled {
                        if let Some(m) = pc.sm.last_meta().get(i) {
                            ImeView::set_str(
                                &mut v.candidates[i].meta,
                                &format!("[{:.3} {}/{}]", m.score, m.family, m.source),
                            );
                        }
                    }
                }
                ImeView::set_str(&mut v.preedit_text, &pc.sm.preedit);
                v.preedit_cursor = pc.sm.cursor as u32;
                v
            })
            .unwrap_or_else(ImeView::empty)
    }

    /// Current pinyin buffer for the default context.
    pub fn buffer(&self) -> String {
        self.contexts
            .lock()
            .unwrap()
            .get(&DEFAULT_CTX)
            .map(|pc| pc.sm.buffer.clone())
            .unwrap_or_default()
    }

    /// Current candidates for the default context.
    pub fn candidates(&self) -> Vec<String> {
        self.contexts
            .lock()
            .unwrap()
            .get(&DEFAULT_CTX)
            .map(|pc| pc.sm.candidates.clone())
            .unwrap_or_default()
    }

    /// Current candidates with full detail (source, score) for debugging.
    /// When the state machine is in Snippet state with fresh candidates, those
    /// are returned directly (they were produced by the Matcher→Expander path,
    /// not the scorer). Otherwise re-runs the scorer on the current buffer.
    pub fn candidates_detailed(&self) -> Vec<crate::family::RankedCandidate> {
        let map = self.contexts.lock().unwrap();
        let Some(pc) = map.get(&DEFAULT_CTX) else {
            return Vec::new();
        };
        // Snippet state (命令组合):candidates 来自命令预测 / 补全,不是 scorer。
        // 直接返回,让 #asr 语音 / #date 日期 / 补全提示正确显示。
        if pc.sm.state == crate::fsm::state::ComposeState::Snippet && pc.sm.candidates_fresh {
            let family: &'static str = pc
                .sm
                .active_command
                .as_ref()
                .map(|m| {
                    if m.name().is_empty() {
                        "snippet"
                    } else {
                        "magic"
                    }
                })
                .unwrap_or("magic");
            return pc
                .sm
                .candidates
                .iter()
                .map(|c| crate::family::RankedCandidate {
                    text: c.clone(),
                    score: 1.0,
                    family,
                    source: "exact",
                })
                .collect();
        }
        let ctx = pc.text_context.clone();
        let buffer = pc.sm.buffer.clone();
        drop(map);
        use crate::fsm::state::StepEnv;
        // 与状态机同序:单字母置顶规则(promote_single_letter)在这里同样
        // 生效 —— 否则 detailed 镜像与机器候选列表顺序分叉。
        crate::fsm::state::promote_single_letter(
            &buffer,
            self.dispatcher.scorer().rank_detailed(&buffer, &ctx),
        )
    }

    /// Manually set the text context (simulates pre-filled text).
    pub fn set_context(&mut self, text: &str) {
        self.contexts
            .lock()
            .unwrap()
            .entry(DEFAULT_CTX)
            .or_insert_with(|| PerContext::with_page_size(self.page_size, self.candidate_meta))
            .text_context
            .update(text);
    }

    /// Load an external dictionary into the PinyinFamily's phrase book.
    /// Supports TSV (`pinyin\tword`) and JSON (`[{"pinyin":"...","text":"..."}]`).
    /// Returns number of entries loaded.
    pub fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher
            .scorer()
            .load_dict_to("pinyin", path)
            .unwrap_or_else(|| {
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
        match PersistenceManager::open(path) {
            Ok(pm) => {
                pm.warm_all(&self.dispatcher);
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

    /// 记录最近一次提交文本的 UTF-8 字符数(`#del` 的 `del_len` 选项读它)。
    fn record_last_commit_len(&self, committed: &str) {
        *self.magic.resources().last_commit_len.lock().unwrap() = committed.chars().count() as u32;
    }

    /// 提交文本是纯 ASCII 字母数字(如 cd)时,学入英文家族 user 层
    /// (英文自生词)。汉字/emoji/符号不触发。Enter 强选 raw 的主路径。
    fn learn_english_if_ascii(&self, committed: &str) {
        if !committed.is_empty() && committed.chars().all(|c| c.is_ascii_alphanumeric()) {
            self.dispatcher.record_english_word(committed);
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
        self.provider.set(name, value);
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
            use crate::fsm::state::ComposeState;
            // 排查流式不刷新:每个 drain 是否到这里、state/has_member 是否正常。
            tracing::info!(
                ctx,
                state = ?pc.sm.state,
                has_member = pc.sm.active_command.is_some(),
                "magic_tick_ctx"
            );
            if pc.sm.state != ComposeState::Snippet {
                return None; // not composing a command for this ctx — common
            }
            // The member is taken out so its tick can freely mutate the state
            // machine, then put back (the member may have exited itself).
            let mut member = pc.sm.active_command.take()?;
            let new_preds = member.tick(&mut pc.sm, disp);
            // Live 成员的 tick 当前返回 None(由 listener 主动 refresh_ui 触发);
            // 但 frontend 拉 magic_tick 时仍要拿到最新候选 —— 重新调 predict 一次。
            let preds = new_preds.unwrap_or_else(|| {
                let input = pc.sm.buffer.clone();
                member.predict(ctx, &input, disp)
            });
            pc.sm.active_command = Some(member);
            pc.sm.magic_predictions = preds;
            pc.table.sync_from(&pc.sm);
            let view = pc.sm.rebuild_magic_view();
            let top = if view.candidate_count > 0 {
                ImeView::str_field(&view.candidates[0].text)
            } else {
                ""
            };
            // 排查"只显示半句":top 是截断前的完整候选文本 —— 若 top 是整句而
            // 面板只显示半句,就是前端截断;若 top 本身就半句,则是折叠/识别问题。
            tracing::info!(ctx, count = view.candidate_count, top, "magic_tick_ctx → view");
            Some(view)
        })
    }

    /// ctx 上是否还有**活跃的 #asr 会话**。前端(`FcitxFrontend::refresh_ui`)
    /// 同步查它来告诉 voice server"这次刷新会不会被主循环接受";voice server
    /// 据此在失败时放弃(`active_ctx = -1`)。
    ///
    /// 线程安全:`contexts` 由 `Mutex` 保护,主线程写、I/O 线程读,无竞争。
    pub fn is_voice_ctx_alive(&self, ctx: usize) -> bool {
        use crate::fsm::state::ComposeState;
        let map = self.contexts.lock().unwrap();
        let alive = map.get(&ctx).is_some_and(|pc| {
            pc.sm.state == ComposeState::Snippet
                && pc.sm
                    .active_command
                    .as_ref()
                    .is_some_and(|m| m.name() == "asr")
        });
        let detail = map.get(&ctx).map(|pc| {
            let state = if pc.sm.state == ComposeState::Snippet {
                "Snippet"
            } else {
                "other"
            };
            let member = pc
                .sm
                .active_command
                .as_ref()
                .map(|m| m.name().to_string())
                .unwrap_or_else(|| "-".into());
            format!("state={state} member={member}")
        });
        tracing::debug!(
            ctx,
            alive,
            detail = detail.unwrap_or_else(|| "no-context".into()),
            "is_voice_ctx_alive"
        );
        alive
    }

    /// Load an English user dictionary from a TSV file.
    /// All words get max priority (10000).
    pub fn load_en_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher.load_en_user_dict(path)
    }

    /// Load the emoji keyword table (CLDR-generated `emoji.tsv`):
    /// `keyword<TAB>emoji`, overriding the embedded base for the same keyword.
    pub fn load_emoji_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher
            .scorer()
            .load_dict_to("emoji", path)
            .unwrap_or_else(|| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "emoji family not found",
                ))
            })
    }

    /// Load the user emoji mapping (`emoji_user.tsv`) — overrides everything
    /// loaded before for the same keyword.
    pub fn load_emoji_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher
            .scorer()
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
        self.dispatcher.load_en_dict(path)
    }
}

impl Default for ImeEngine {
    fn default() -> Self {
        ImeEngine::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn eng() -> ImeEngine {
        ImeEngine::new()
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
            Box::new(NoVars),
            vec![SnippetEntry {
                name: "hello".into(),
                comment: String::new(),
                params: Vec::new(),
                template: "Hello, my name is $name.".into(),
            }],
            crate::scoring::ScoringConfig::default(),
            Arc::new(crate::frontend::NoopFrontend::default()),
            DEFAULT_VOICE_AURA_BASE.to_string(),
            crate::io_thread::DEFAULT_IDLE_TIMEOUT_SECS,
            Vec::new(),
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

    // ── #req member ──────────────────────────────────────────────────────

    /// Scripted fetcher — records the requested URL and returns a canned result.
    #[derive(Clone)]
    struct FakeFetcher {
        result: Result<String, String>,
        urls: Arc<Mutex<Vec<String>>>,
    }

    impl ReqFetcher for FakeFetcher {
        fn post(&self, url: &str, _body: &str) -> Result<String, String> {
            self.urls.lock().unwrap().push(url.to_string());
            self.result.clone()
        }
    }

    /// Poll until the worker thread's result lands (or fail). 结果可能在两种
    /// 时刻出现:pick 后的 predict 已读到 Done(worker 太快,候选已是 body),
    /// 或 magic_tick 检测到版本变化。两种都算落地。
    /// Budget 30s with fine-grained polling — under full parallel test load
    /// (150+ tests, starved CI containers) the spawned worker thread can be
    /// delayed tens of seconds; the tests assert correctness, not speed.
    fn wait_req_tick(e: &ImeEngine) {
        for _ in 0..15_000 {
            if e.magic_tick().is_some() {
                return;
            }
            // 结果已在 predict 里落地(worker 快于重查)—— 候选不再是
            // "回车请求…" / "请求中…"。
            if e.candidates()
                .first()
                .map(|c| !c.contains("请求中") && !c.contains("回车请求"))
                .unwrap_or(false)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("req result never landed");
    }
}
