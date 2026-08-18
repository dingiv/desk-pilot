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
use std::time::Instant;

use crate::dispatcher::Dispatcher;
use crate::family::InputContext;
use crate::family::magic::{MagicFamily, ReqFetcher};
use crate::router::{StateMachineTable, StateFlags};
// 统一键事件由输入路由层定义(旧名 InputEvent;构造器同名,测试平移)。
pub use crate::router::KeyEvent;
use crate::store::PersistenceManager;
use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};

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
        PerContext { sm, table: StateMachineTable::new(), text_context: InputContext::new() }
    }
}

// ── WaitState (async #wait demo) ────────────────────────────────────────

struct WaitState {
    trigger_time: Instant,
    chars: Vec<(u64, char)>,
}

// ── ImeEngine ───────────────────────────────────────────────────────────

const DEFAULT_CTX: usize = 0; // used by single-context convenience methods

/// Self-contained IME engine. Manages the dispatcher, per-context state
/// machines, input context, and async waits.
pub struct ImeEngine {
    dispatcher: Dispatcher,
    contexts: Mutex<HashMap<usize, PerContext>>,
    async_waits: Mutex<HashMap<usize, WaitState>>,
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
    provider: Arc<dyn crate::expander::VariableProvider>,
    /// 候选每页条数(swift-ime.yaml → input.page_size;默认 7)。传给每个新建的
    /// StateMachine —— 之前写死在 `StateMachine::new` 里(FIXME)。
    page_size: u32,
    /// 调试模式:候选词显示提供者与权重(swift-ime.yaml → debug.candidate_meta)。
    candidate_meta: bool,
}

impl ImeEngine {
    /// Create a new engine with all default prediction families, built-in
    /// snippet triggers, and the embedded base phrase dictionary.
    pub fn new() -> Self {
        Self::with_pinyin_weights(crate::family::pinyin::PinyinWeights::default())
    }

    /// Create engine with custom pinyin family weights (from config file).
    pub fn with_pinyin_weights(weights: crate::family::pinyin::PinyinWeights) -> Self {
        Self::with_config(
            weights,
            crate::family::english::EnglishWeights::default(),
            Box::new(crate::expander::DefaultProvider),
            Vec::new(),
            crate::scoring::ScoringConfig::default(),
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
    pub fn with_config(
        pinyin_weights: crate::family::pinyin::PinyinWeights,
        english_weights: crate::family::english::EnglishWeights,
        provider: Box<dyn crate::expander::VariableProvider>,
        extra_snippets: Vec<(String, String)>,
        scoring: crate::scoring::ScoringConfig,
    ) -> Self {
        // Magic command entries are generated from the member registry (#asr, #flush,
        // #submit, #req, #date, #password …) — adding a command = one member, nothing
        // here. `/`-snippets are now the empty-name snippet magic command (`#/sig`);
        // the `#wait` async demo stays a plain matcher entry.
        let magic = Arc::new(MagicFamily::new());
        let mut entries = magic.matcher_entries();
        entries.push(("#wait".into(), "__WAIT_DEMO__".into()));
        let matcher = crate::Matcher::new(entries);
        // 片段注册表:内置 + 配置片段;名字去掉前导 `/`(触发名 `/sig` → `sig`,
        // 调用 `#/sig`)。
        let mut snippets: Vec<(String, String)> = vec![
            ("greet".into(), "你好，我是 AI 秘书，请问有什么可以帮你的？".into()),
            ("sig".into(), "Best regards,\nAlice".into()),
        ];
        for (trigger, expand) in extra_snippets {
            let name = trigger.strip_prefix('/').unwrap_or(&trigger).to_string();
            snippets.push((name, expand));
        }
        magic.set_snippets(snippets);
        // Shared with the dispatcher's expander — `set_variable` writes through the same Arc.
        let provider: Arc<dyn crate::expander::VariableProvider> = Arc::from(provider);
        let expander = crate::Expander::new(Arc::clone(&provider));
        let engine = ImeEngine {
            dispatcher: Dispatcher::with_config(matcher, expander, Arc::clone(&magic), pinyin_weights, english_weights, scoring),
            contexts: Mutex::new(HashMap::new()),
            async_waits: Mutex::new(HashMap::new()),
            persistence: Mutex::new(None),
            magic,
            provider,
            page_size: 7,
            candidate_meta: false,
        };
        // Load embedded base dictionary (5KB, compiled into binary).
        let count = engine.dispatcher.scorer().family("pinyin")
            .map(|f| f.load_dict_bytes(Self::EMBEDDED_BASE_DICT))
            .unwrap_or(0);
        if count > 0 {
            tracing::info!(count, "loaded embedded base dictionary");
        }
        engine
    }

    /// Embedded base phrase dictionary (TSV format), compiled into the binary.
    const EMBEDDED_BASE_DICT: &[u8] = include_bytes!("../../../apps/swift-ime/assets/dict/base.tsv");

    // ── ctx helpers ─────────────────────────────────────────────────────

    fn with_ctx<T>(&self, ctx: usize, f: impl FnOnce(&Dispatcher, &mut PerContext) -> T) -> T {
        // FIXME: 一处不必要的 unwrap
        let mut map = self.contexts.lock().unwrap();
        let pc = map.entry(ctx).or_insert_with(|| PerContext::with_page_size(self.page_size, self.candidate_meta));
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

    /// 临时关闭/恢复 pinyin 家族的上下文感知(swift-ime.yaml → input.context_aware)。
    /// 关闭后 recency / 整词联想加成全部跳过,候选排序纯频率驱动。
    pub fn set_context_aware(&mut self, on: bool) {
        self.dispatcher.set_pinyin_context_aware(on);
    }

    /// 候选每页条数(默认 7)。frontend 启动时调用(swift-ime.yaml → input.page_size)。
    /// 已存在的 context 立即生效,后续新建的 context 沿用新值。
    pub fn set_page_size(&mut self, page_size: u32) {
        if page_size == 0 { return; }
        self.page_size = page_size;
        for pc in self.contexts.lock().unwrap().values_mut() {
            pc.sm.candidate_page_size = page_size as usize;
        }
    }

    fn remove_ctx(&self, ctx: usize) {
        self.contexts.lock().unwrap().remove(&ctx);
        self.async_waits.lock().unwrap().remove(&ctx);
    }

    // ── Multi-context API (used by fcitx5 C ABI) ────────────────────────

    /// **统一键入口**:所有前端把键(含特殊键与 Ctrl/Shift/Alt 修饰状态)
    /// 忠实地转成 [`KeyEvent`] 喂到这里。输入路由层(状态机表)查表决定
    /// 这枚键属于输入法还是应用,驱动组合状态机迁移,返回带 action 位
    /// 标志的视图 —— 外界按 [`action`](crate::platform::action) 反应即可,
    /// 不再自行拦截任何键。
    pub fn key_ctx(&self, ctx: usize, key: KeyEvent) -> ImeView {
        self.with_ctx(ctx, |disp, pc| {
            pc.sm.context = pc.text_context.clone();
            let mut view = pc.table.route(&mut pc.sm, key, disp);
            let committed = ImeView::str_field(&view.commit_text);
            if !committed.is_empty() {
                pc.text_context.update(committed);
                // Record bigram: prev_word → committed_word (both SQLite + in-memory).
                self.dispatcher.record_commit(committed);
                self.learn_english_if_ascii(committed);
            }
            // #wait demo interceptor.
            // FIXME: 删除 demo 代码
            if ImeView::str_field(&view.commit_text) == "__WAIT_DEMO__" {
                self.async_waits.lock().unwrap().insert(ctx, WaitState {
                    trigger_time: Instant::now(),
                    chars: vec![(0, 'a'), (1000, 'b'), (2000, 'c')],
                });
                view.commit_text = [0u8; 512];
                ImeView::set_str(&mut view.preedit_text, "a");
                view.preedit_cursor = 1;
            }
            view
        })
    }

    /// 当前输入上下文的状态标志位(状态机表)。TUI 状态栏 / 调试用。
    pub fn state_flags_ctx(&self, ctx: usize) -> StateFlags {
        self.contexts.lock().unwrap()
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
                self.dispatcher.record_commit(committed);
                self.learn_english_if_ascii(committed);
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
        let Some(pc) = map.get(&ctx) else { return ImeView::empty() };
        // 候选(英文按键入大小写回填)优先,否则提交原始输入 raw_buffer。
        let text = pc.sm.candidates.first()
            .map(|c| crate::state::apply_input_casing(c, &pc.sm.raw_buffer))
            .unwrap_or_else(|| pc.sm.raw_buffer.clone());
        let mut v = ImeView::empty();
        if !text.is_empty() {
            ImeView::set_str(&mut v.commit_text, &text);
        }
        v
    }

    /// Poll async state (#wait demo). Returns (0=nothing, 1=preedit, 2=commit).
    pub fn poll_async_ctx(&self, ctx: usize) -> (i32, ImeView) {
        let mut waits = self.async_waits.lock().unwrap();
        let Some(ws) = waits.get(&ctx) else { return (0, ImeView::empty()) };
        let ms = ws.trigger_time.elapsed().as_millis() as u64;
        let text: String = ws.chars.iter().filter(|(t,_)| *t <= ms).map(|(_,c)| *c).collect();
        let mut v = ImeView::empty();
        if ms > 2100 {
            waits.remove(&ctx);
            ImeView::set_str(&mut v.commit_text, &text);
            (2, v)
        } else {
            ImeView::set_str(&mut v.preedit_text, &text);
            v.preedit_cursor = text.len() as u32;
            (1, v)
        }
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

    /// Poll async state for the default context.
    pub fn poll_async(&self) -> (i32, ImeView) {
        self.poll_async_ctx(DEFAULT_CTX)
    }

    /// Rebuild the ImeView from current state (for display after navigation).
    /// Returns the full UI snapshot without processing a key event.
    pub fn view(&self) -> ImeView {
        self.contexts.lock().unwrap()
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
                        if let Some((score, fam, src)) = pc.sm.last_meta().get(i) {
                            ImeView::set_str(&mut v.candidates[i].meta,
                                &format!("[{score:.3} {fam}/{src}]"));
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
        self.contexts.lock().unwrap()
            .get(&DEFAULT_CTX).map(|pc| pc.sm.buffer.clone()).unwrap_or_default()
    }

    /// Current candidates for the default context.
    pub fn candidates(&self) -> Vec<String> {
        self.contexts.lock().unwrap()
            .get(&DEFAULT_CTX).map(|pc| pc.sm.candidates.clone()).unwrap_or_default()
    }

    /// Current candidates with full detail (source, score) for debugging.
    /// When the state machine is in Snippet state with fresh candidates, those
    /// are returned directly (they were produced by the Matcher→Expander path,
    /// not the scorer). Otherwise re-runs the scorer on the current buffer.
    pub fn candidates_detailed(&self) -> Vec<crate::family::RankedCandidate> {
        let map = self.contexts.lock().unwrap();
        let Some(pc) = map.get(&DEFAULT_CTX) else { return Vec::new() };
        // Snippet/Magic state: candidates come from the Matcher trie / live member,
        // not the scorer. Return them directly so #asr / #date expansions appear
        // correctly. For Magic, the member's full commit texts (voice sentences /
        // req bodies stay whole — each frontend truncates rows for its own display).
        if pc.sm.state == crate::state::ComposeState::Magic && pc.sm.candidates_fresh {
            let family: &'static str = pc.sm.magic_member.as_ref().map(|m| m.name()).unwrap_or("magic");
            let texts: Vec<String> = match pc.sm.magic_member.as_ref() {
                Some(m) => m.candidate_texts(&pc.sm),
                None => pc.sm.candidates.clone(),
            };
            return texts.iter().map(|c| crate::family::RankedCandidate {
                text: c.clone(),
                score: 1.0,
                family,
                source: "exact",
            }).collect();
        }
        if pc.sm.state == crate::state::ComposeState::Snippet && pc.sm.candidates_fresh {
            let source = if pc.sm.buffer.starts_with('#') { "magic" } else { "snippet" };
            return pc.sm.candidates.iter().map(|c| crate::family::RankedCandidate {
                text: c.clone(),
                score: 1.0,
                family: source,
                source: "exact",
            }).collect();
        }
        let ctx = pc.text_context.clone();
        let buffer = pc.sm.buffer.clone();
        drop(map);
        use crate::state::StepEnv;
        self.dispatcher.scorer().rank_detailed(&buffer, &ctx)
    }

    /// Manually set the text context (simulates pre-filled text).
    pub fn set_context(&mut self, text: &str) {
        self.contexts.lock().unwrap()
            .entry(DEFAULT_CTX).or_insert_with(|| PerContext::with_page_size(self.page_size, self.candidate_meta))
            .text_context.update(text);
    }

    /// Load an external dictionary into the PinyinFamily's phrase book.
    /// Supports TSV (`pinyin\tword`) and JSON (`[{"pinyin":"...","text":"..."}]`).
    /// Returns number of entries loaded.
    pub fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher.scorer().load_dict_to("pinyin", path)
            .unwrap_or_else(|| Err(std::io::Error::new(
                std::io::ErrorKind::NotFound, "pinyin family not found")))
    }

    /// Initialize the unified persistence manager. Call once at startup —
    /// warms EVERY persisted user model (bigrams, phrases, recency ring, L0)
    /// into the in-memory stores, then families double-write from here on.
    pub fn init_store(&self, path: &str) {
        match PersistenceManager::open(path) {
            Ok(pm) => {
                pm.warm_all(&self.dispatcher);
                eprintln!("[swift-ime] weight store: {} phrases, {} en-words from {path}",
                    pm.phrase_count(), pm.en_user_count());
                *self.persistence.lock().unwrap() = Some(pm);
            }
            Err(e) => eprintln!("[swift-ime] weight store open failed: {e}"),
        }
    }

    /// 提交文本是纯 ASCII 字母数字(如 cd)时,学入英文家族 user 层
    /// (英文自生词)。汉字/emoji/符号不触发。Enter 强选 raw 的主路径。
    fn learn_english_if_ascii(&self, committed: &str) {
        if !committed.is_empty()
            && committed.chars().all(|c| c.is_ascii_alphanumeric())
        {
            self.dispatcher.record_english_word(committed);
        }
    }

    /// Attach the voice buffer so `#asr` resolves to live voice recognition
    /// text from the aura daemon SSE stream. Call once at startup after the SSE
    /// client has been spawned. Routed to the magic registry's shared slot —
    /// every per-context `VoiceMember` instance reads it.
    pub fn set_asr_buffer(&self, buf: std::sync::Arc<crate::asr_buffer::AsrBuffer>) {
        self.dispatcher.set_asr_buffer(buf);
    }

    /// `#req` backend base URL (default `http://127.0.0.1:14555/api`).
    /// `#req/news?query=soccer` → `GET {base}/news?query=soccer`.
    pub fn set_req_base(&self, base: &str) {
        self.magic.set_req_base(base);
    }

    /// Inject an HTTP fetcher for `#req` (tests use a fake; the production default
    /// is a reqwest client behind ime-core's `http` feature).
    pub fn set_req_fetcher(&self, fetcher: Arc<dyn ReqFetcher>) {
        self.magic.set_req_fetcher(fetcher);
    }

    /// Update a snippet variable's value at runtime — e.g. the fcitx5 frontend
    /// pushes clipboard changes here (via the C ABI) so `$CLIPBOARD` templates
    /// expand to the current text. Providers that don't support updates ignore it.
    pub fn set_variable(&self, name: &str, value: &str) {
        self.provider.set(name, value);
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
            use crate::state::ComposeState;
            if pc.sm.state != ComposeState::Magic {
                return None; // not in a live command for this ctx — common
            }
            // The member is taken out so its tick can freely mutate the state
            // machine, then put back (the member may have exited itself).
            let mut member = pc.sm.magic_member.take()?;
            let changed = member.tick(&mut pc.sm, disp);
            pc.sm.magic_member = Some(member);
            // The member rebuilt its candidates — re-assemble the preview tail.
            pc.sm.assemble_magic_tail(disp);
            // 路由之外的 sm 变更 —— 状态机表重新同步。
            pc.table.sync_from(&pc.sm);
            changed
        })
    }

    /// Load an English user dictionary from a TSV file.
    /// All words get max priority (10000).
    pub fn load_en_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher.load_en_user_dict(path)
    }

    /// Load the emoji keyword table (CLDR-generated `emoji.tsv`):
    /// `keyword<TAB>emoji`, overriding the embedded base for the same keyword.
    pub fn load_emoji_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher.scorer().load_dict_to("emoji", path)
            .unwrap_or_else(|| Err(std::io::Error::new(
                std::io::ErrorKind::NotFound, "emoji family not found")))
    }

    /// Load the user emoji mapping (`emoji_user.tsv`) — overrides everything
    /// loaded before for the same keyword.
    pub fn load_emoji_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher.scorer().load_user_dict_to("emoji", path)
            .unwrap_or_else(|| Err(std::io::Error::new(
                std::io::ErrorKind::NotFound, "emoji family not found")))
    }

    /// Load an external English dictionary (auto-detect type, normalize, cache).
    pub fn load_en_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher.load_en_dict(path)
    }
}

impl Default for ImeEngine {
    fn default() -> Self { ImeEngine::new() }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn eng() -> ImeEngine { ImeEngine::new() }

    #[test]
    fn type_pinyin_and_commit() {
        let mut e = eng();
        for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
        assert!(e.candidates().iter().any(|c| c.contains("你好")));
        let v = e.predict(KeyEvent::space());
        assert!(ImeView::str_field(&v.commit_text).contains("你"));
    }

    #[test]
    fn uppercase_english_predicts_as_lowercase_and_commits_cased() {
        // 大写 E 视作小写 e 预测(词典候选 english 出现),提交保留 English。
        let mut e = eng();
        for c in "English".chars() { e.predict(KeyEvent::char(c)); }
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
        for c in "English".chars() { e.predict(KeyEvent::char(c)); }
        let v = e.predict(KeyEvent::enter());
        assert_eq!(ImeView::str_field(&v.commit_text), "English");
    }

    #[test]
    fn prefix_case_applied_to_completion() {
        // "Engli" → 补全 english:前缀回填大小写,补全段( sh )保持小写。
        let mut e = eng();
        for c in "Engli".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c == "english"), "{cands:?}");
        let idx = cands.iter().position(|c| c == "english").unwrap();
        let v = e.select_candidate(idx);
        assert_eq!(ImeView::str_field(&v.commit_text), "English");
    }

    #[test]
    fn all_caps_english_commits_all_caps() {
        let mut e = eng();
        for c in "ENGLISH".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        let idx = cands.iter().position(|c| c == "english").expect("english candidate");
        let v = e.select_candidate(idx);
        assert_eq!(ImeView::str_field(&v.commit_text), "ENGLISH");
    }

    #[test]
    fn lowercase_english_unchanged() {
        let mut e = eng();
        for c in "english".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        let idx = cands.iter().position(|c| c == "english").expect("english candidate");
        let v = e.select_candidate(idx);
        assert_eq!(ImeView::str_field(&v.commit_text), "english");
    }

    #[test]
    fn incremental_composition() {
        let mut e = eng();
        for c in "lizhengming".chars() { e.predict(KeyEvent::char(c)); }
        let li = e.candidates().iter().position(|c| c == "李").unwrap();
        e.select_candidate(li);
        let zheng = e.candidates().iter().position(|c| c == "正").unwrap();
        e.select_candidate(zheng);
        let ming = e.candidates().iter().position(|c| c == "明").unwrap();
        let v = e.select_candidate(ming);
        assert_eq!(ImeView::str_field(&v.commit_text), "李正明");
    }

    #[test]
    fn snippet_expansion() {
        // 内置片段 greet 经 `#/greet` 展开。
        let mut e = eng();
        for c in "#/greet".chars() { e.predict(KeyEvent::char(c)); }
        let v = e.predict(KeyEvent::space());
        assert_eq!(
            ImeView::str_field(&v.commit_text),
            "你好，我是 AI 秘书，请问有什么可以帮你的？"
        );
    }

    #[test]
    fn snippet_query_params_inject_template_variables() {
        // #/hello?name=Mike → 查询参数注入模板变量 $name。
        use crate::expander::VariableProvider;
        #[derive(Clone)]
        struct NoVars;
        impl VariableProvider for NoVars {
            fn resolve(&self, _name: &str) -> Option<String> { None }
        }
        let e = ImeEngine::with_config(
            crate::family::pinyin::PinyinWeights::default(),
            crate::family::english::EnglishWeights::default(),
            Box::new(NoVars),
            vec![("/hello".into(), "Hello, my name is $name.".into())],
            crate::scoring::ScoringConfig::default(),
        );
        let mut e = e;
        for c in "#/hello?name=Mike".chars() { e.predict(KeyEvent::char(c)); }
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "Hello, my name is Mike.");
    }

    #[test]
    fn snippet_unknown_name_shows_hint_and_commits_empty() {
        // 未知片段名 → 候选"未知片段 /nope",Space 空提交。
        let mut e = eng();
        for c in "#/nope".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("未知片段")), "{cands:?}");
        let v = e.predict(KeyEvent::space());
        assert!(ImeView::str_field(&v.commit_text).is_empty(), "unknown snippet commits nothing");
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
    fn asr_voice_anchor_live_then_final_then_commit() {
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true); // simulate a live aura link
        e.set_asr_buffer(Arc::clone(&buf));

        // type #asr → Voice mode, preview candidate (no voice data yet)
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("语音识别中")), "preview when empty: {cands:?}");

        // voice streams → live candidate appears; magic_tick rebuilds it
        buf.set_live("你好");
        assert!(e.magic_tick().is_some(), "tick rebuilds after set_live");
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c == "你好"), "live candidate shown: {cands:?}");

        // Stage2 final → becomes #1
        buf.push_final("你好世界");
        assert!(e.magic_tick().is_some());
        let cands = e.candidates();
        assert_eq!(cands.first(), Some(&"你好世界".to_string()), "final is #1: {cands:?}");

        // a second tick with no change → None
        assert!(e.magic_tick().is_none(), "no rebuild when version unchanged");

        // space commits #1 → 上屏; back to idle
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "你好世界");
        assert!(e.candidates().is_empty(), "candidates cleared after commit");
    }

    #[test]
    fn asr_voice_escape_cancels() {
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true); // simulate a live aura link
        e.set_asr_buffer(Arc::clone(&buf));
        buf.push_final("识别文本");
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        e.magic_tick();
        // Escape → cancel (no commit), back to idle
        let v = e.predict(KeyEvent::escape());
        assert!(ImeView::str_field(&v.commit_text).is_empty(), "escape commits nothing");
        assert!(e.candidates().is_empty(), "cleared after escape");
    }

    #[test]
    fn asr_path_en_enters_translate_mode_stub() {
        // #asr/en → /en 传入家族内部,进入英文翻译模式(翻译实现留空,路径已就绪)。
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true);
        e.set_asr_buffer(Arc::clone(&buf));

        for c in "#asr/en".chars() { e.predict(KeyEvent::char(c)); }
        let view = e.view();
        let preedit = ImeView::str_field(&view.preedit_text);
        assert!(preedit.contains("#asr/en"), "arg echoed in preedit: {preedit}");
        assert!(preedit.contains("英文翻译"), "translate marker: {preedit}");
    }

    #[test]
    fn asr_unknown_path_handled_by_family() {
        // #asr/unknown → 家族内部自行处理(打未知参数标记,不崩溃)。
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true);
        e.set_asr_buffer(Arc::clone(&buf));

        for c in "#asr/unknown".chars() { e.predict(KeyEvent::char(c)); }
        let view = e.view();
        let preedit = ImeView::str_field(&view.preedit_text);
        assert!(preedit.contains("#asr/unknown"), "arg echoed: {preedit}");
        assert!(preedit.contains("未知参数"), "unknown marker: {preedit}");
    }

    #[test]
    fn asr_query_num_commits_latest_n_finals() {
        // #asr?num=2 → 提交语音队列最新两条定稿。
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true);
        buf.push_final("第一句");
        buf.push_final("第二句");
        buf.push_final("第三句");
        e.set_asr_buffer(Arc::clone(&buf));

        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        let mut last = ImeView::empty();
        for c in "?num=2".chars() { last = e.predict(KeyEvent::char(c)); }
        let committed = ImeView::str_field(&last.commit_text);
        assert_eq!(committed, "第三句\n第二句", "latest 2 finals: {committed:?}");
        assert!(e.candidates().is_empty(), "session ended after commit");
    }

    #[test]
    fn magic_prefix_space_completes_into_command_not_raw() {
        // `#as` + Space → behaves like typing `#asr` (enters Magic mode), NOT committing "#as".
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true); // simulate a live aura link
        e.set_asr_buffer(Arc::clone(&buf));

        for c in "#as".chars() { e.predict(KeyEvent::char(c)); }
        // Hint: completion candidate (#asr) + raw (#as).
        let cands = e.candidates();
        assert!(cands.contains(&"#asr".to_string()), "completion hint shown: {cands:?}");
        assert!(cands.contains(&"#as".to_string()), "raw kept as fallback: {cands:?}");

        // Space → enters Voice mode (like #asr), does NOT commit "#as".
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "", "space must not commit raw #as");
        assert!(e.candidates().iter().any(|c| c.contains("语音识别中")), "now in asr mode: {:?}", e.candidates());
    }

    #[test]
    fn magic_preview_panel_has_member_tail_and_rollback() {
        // Preview state (Magic) candidate panel = [member candidates…] + [tail…] + [rollback].
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true); // simulate a live aura link
        e.set_asr_buffer(Arc::clone(&buf));
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        // Member placeholder is #1; rollback (#asr) is the LAST candidate.
        let cands = e.candidates();
        assert!(cands.first().map(|c| c.contains("语音识别中")).unwrap_or(false), "member candidate first: {cands:?}");
        assert_eq!(cands.last(), Some(&"#asr".to_string()), "rollback is last: {cands:?}");
    }

    #[test]
    fn magic_rollback_space_commits_trigger_text() {
        // In preview, Space on the LAST (rollback) candidate commits the raw trigger.
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true); // simulate a live aura link
        e.set_asr_buffer(Arc::clone(&buf));
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        // Move highlight to the rollback (last candidate), then Space.
        let n = e.candidates().len();
        for _ in 0..(n - 1) {
            e.key(KeyEvent { kind: crate::router::KeyKind::Down, ctrl: false, shift: false, alt: false });
        }
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "#asr", "rollback commits #asr");
        assert!(e.candidates().is_empty(), "exited preview");
    }

    #[test]
    fn unknown_magic_space_commits_raw() {
        // `#x` (no magic match) → raw kept as candidate; Space commits it.
        let mut e = eng();
        for c in "#x".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        assert_eq!(cands, vec!["#x".to_string()], "raw only: {cands:?}");

        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "#x", "space commits raw #x");
        assert!(e.candidates().is_empty(), "cleared after commit");
    }

    #[test]
    fn magic_enter_commits_trigger_text() {
        // `#asr` complete match → Magic mode; Enter force-commits "#asr" (回车强制上屏).
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true); // simulate a live aura link
        e.set_asr_buffer(Arc::clone(&buf));

        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        assert!(e.candidates().iter().any(|c| c.contains("语音识别中")), "in asr mode: {:?}", e.candidates());

        let v = e.predict(KeyEvent::enter());
        assert_eq!(ImeView::str_field(&v.commit_text), "#asr", "Enter force-commits the trigger");
        assert!(e.candidates().is_empty(), "exited magic mode");
    }

    #[test]
    fn asr_voice_long_sentence_keeps_full_text_for_frontends() {
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        // A long sentence (~72 bytes, well under the 128-byte C ABI candidate slot).
        // The engine passes full texts — each frontend truncates rows for its own
        // display (fcitx5 panel), while the TUI shows everything.
        let long = "这是一句相当长的话，超出显示上限。".repeat(2); // ~72 bytes
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true);
        buf.push_final(&long);
        let mut e = eng();
        e.set_asr_buffer(Arc::clone(&buf));
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        e.magic_tick();
        let cands = e.candidates();
        assert_eq!(cands[0], long, "engine candidate is the full text: {cands:?}");
        // The preedit expands the anchor into the recognized text.
        let v0 = e.view();
        let preedit = ImeView::str_field(&v0.preedit_text);
        assert_eq!(preedit, format!("🎙 #asr {long}"), "preedit shows the voice text");
        // Space commits the FULL text (from voice_full), not any display preview.
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), long);
    }

    #[test]
    fn asr_voice_active_live_is_candidate_1() {
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true); // simulate a live aura link
        e.set_asr_buffer(Arc::clone(&buf));
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        // two settled finals, then a 3rd utterance starts streaming (live)
        buf.push_final("第一句");
        buf.push_final("第二句");
        buf.set_live("第三句流式中");
        e.magic_tick();
        let cands = e.candidates();
        // live (the active one) is #1; then finals newest→oldest
        assert_eq!(cands[0], "第三句流式中", "live is #1: {cands:?}");
        assert_eq!(cands[1], "第二句", "newest final is #2");
        assert_eq!(cands[2], "第一句", "older final is #3");

        // when the live utterance settles, it graduates to #1 (still newest)
        buf.push_final("第三句定稿");
        e.magic_tick();
        let cands = e.candidates();
        assert_eq!(cands[0], "第三句定稿", "settled live becomes #1: {cands:?}");
        assert_eq!(cands[1], "第二句");
    }

    #[test]
    fn space_commit_records_recency() {
        // Regression: Space-commits used to bypass record_commit — the recency
        // ring was never updated by space-commits (and nothing persisted).
        use crate::store::WeightStore;
        let db = format!("/tmp/swift-ime-space-rec-{}.db", std::process::id());
        let _ = std::fs::remove_file(&db);
        let mut e = eng();
        e.init_store(&db);
        for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
        e.predict(KeyEvent::space()); // commits 你好 via the special-key path
        let store = WeightStore::open(&db).unwrap();
        let rec = store.load_recency();
        assert_eq!(rec.len(), 1, "space-commit must reach the recency table");
        assert_eq!(rec[0].0, "你好", "recorded word: {rec:?}");
        assert!(rec[0].1 > 0, "with a wall-clock timestamp: {rec:?}");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn enter_commits_raw() {
        let mut e = eng();
        for c in "hello".chars() { e.predict(KeyEvent::char(c)); }
        let v = e.predict(KeyEvent::enter());
        assert_eq!(ImeView::str_field(&v.commit_text), "hello");
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
        fn get(&self, url: &str) -> Result<String, String> {
            self.urls.lock().unwrap().push(url.to_string());
            self.result.clone()
        }
    }

    /// Poll `magic_tick` until the worker thread's result lands (or fail).
    /// Budget 30s with fine-grained polling — under full parallel test load
    /// (150+ tests, starved CI containers) the spawned worker thread can be
    /// delayed tens of seconds; the tests assert correctness, not speed.
    /// (10s still tripped on an 11.6s-loaded run — last flake, 2026-08-15.)
    fn wait_req_tick(e: &ImeEngine) {
        for _ in 0..15_000 {
            if e.magic_tick().is_some() { return; }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("req result never landed");
    }

    fn req_eng() -> (ImeEngine, FakeFetcher) {
        let e = eng();
        let fake = FakeFetcher {
            result: Ok("这是本地服务返回的正文内容".into()),
            urls: Arc::new(Mutex::new(Vec::new())),
        };
        e.set_req_fetcher(Arc::new(fake.clone()));
        (e, fake)
    }

    #[test]
    fn req_anchor_hint_then_enter_fires_then_space_commits() {
        let (mut e, fake) = req_eng();
        for c in "#req".chars() { e.predict(KeyEvent::char(c)); }
        // Activation: hint candidate with the full URL
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("http://127.0.0.1:14555/api")), "hint: {cands:?}");

        // Enter fires the request → result lands on the worker thread
        e.predict(KeyEvent::enter());
        wait_req_tick(&e);
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("这是本地服务返回的正文内容")), "body shown: {cands:?}");
        assert_eq!(fake.urls.lock().unwrap().as_slice(), ["http://127.0.0.1:14555/api"], "fired the base URL");

        // Space commits the body; member exits, back to idle
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "这是本地服务返回的正文内容");
        assert!(e.candidates().is_empty(), "cleared after commit");
    }

    #[test]
    fn req_suffix_extends_url() {
        let (mut e, fake) = req_eng();
        for c in "#req/news?query=soccer".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("http://127.0.0.1:14555/api/news?query=soccer")), "hint shows full URL: {cands:?}");

        e.predict(KeyEvent::enter());
        wait_req_tick(&e);
        assert_eq!(fake.urls.lock().unwrap().as_slice(), ["http://127.0.0.1:14555/api/news?query=soccer"], "fired with suffix");
    }

    #[test]
    fn req_long_body_preview_truncates_commit_full() {
        let body = "长正文".repeat(30); // 270 bytes, ≫ 60
        let mut e = eng();
        e.set_req_fetcher(Arc::new(FakeFetcher { result: Ok(body.clone()), urls: Arc::new(Mutex::new(Vec::new())) }));
        for c in "#req".chars() { e.predict(KeyEvent::char(c)); }
        e.predict(KeyEvent::enter());
        wait_req_tick(&e);
        let cands = e.candidates();
        // Preview panel: [member body] + [rollback #req]. The body is #1.
        assert!(cands.len() >= 2, "body + rollback tail: {cands:?}");
        assert!(cands[0].ends_with('…'), "preview truncated with ellipsis: {}", cands[0]);
        assert!(cands[0].chars().count() <= 60, "preview ≤ 60 chars: {}", cands[0]);
        assert_eq!(cands.last(), Some(&"#req".to_string()), "rollback is the last candidate");
        // Space on the member candidate commits the FULL body, not the preview
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), body);
    }

    #[test]
    fn req_failure_shows_error_and_never_commits_it() {
        let mut e = eng();
        e.set_req_fetcher(Arc::new(FakeFetcher { result: Err("HTTP 500".into()), urls: Arc::new(Mutex::new(Vec::new())) }));
        for c in "#req".chars() { e.predict(KeyEvent::char(c)); }
        e.predict(KeyEvent::enter());
        wait_req_tick(&e);
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("请求失败") && c.contains("HTTP 500")), "error shown: {cands:?}");

        // Space on a failed state re-fires (no garbage commit)
        let v = e.predict(KeyEvent::space());
        assert!(ImeView::str_field(&v.commit_text).is_empty(), "error text never committed");
        wait_req_tick(&e);
        // Escape cancels the session
        let v = e.predict(KeyEvent::escape());
        assert!(ImeView::str_field(&v.commit_text).is_empty());
        assert!(e.candidates().is_empty(), "cleared after escape");
    }

    #[test]
    fn req_backspace_edits_suffix_then_exits() {
        let (mut e, _fake) = req_eng();
        for c in "#req/news".chars() { e.predict(KeyEvent::char(c)); }
        // Backspace pops one suffix char
        e.predict(KeyEvent::backspace());
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("/new") && !c.contains("/news")), "suffix edited: {cands:?}");
        // Delete the rest — when the suffix is empty, Backspace exits the member
        for _ in 0..5 { e.predict(KeyEvent::backspace()); }
        assert!(e.candidates().is_empty(), "member exited when suffix emptied: {:?}", e.candidates());
    }

    #[test]
    fn req_digit_extends_url_without_result_commits_with_result() {
        let (mut e, fake) = req_eng();
        for c in "#req/news".chars() { e.predict(KeyEvent::char(c)); }
        // No result yet → digit is a URL character
        e.predict(KeyEvent::char('2'));
        e.predict(KeyEvent::char('0'));
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("/news20")), "digit appended to suffix: {cands:?}");

        // Fire → result lands → digit 1 commits the body
        e.predict(KeyEvent::enter());
        wait_req_tick(&e);
        let v = e.predict(KeyEvent::char('1'));
        assert_eq!(ImeView::str_field(&v.commit_text), "这是本地服务返回的正文内容");
        assert!(fake.urls.lock().unwrap().iter().any(|u| u.ends_with("/news20")), "fired with digits in URL");
    }

    #[test]
    fn req_escape_cancels() {
        let mut e = eng();
        for c in "#req".chars() { e.predict(KeyEvent::char(c)); }
        let v = e.predict(KeyEvent::escape());
        assert!(ImeView::str_field(&v.commit_text).is_empty(), "escape commits nothing");
        assert!(e.candidates().is_empty(), "cleared after escape");
    }

    #[test]
    fn config_snippet_overrides_builtin_and_expands_variables() {
        // A config snippet can override a built-in trigger AND use variables:
        // `/sig` is built-in ("Best regards,\nAlice"); the config version adds
        // $DATE (resolved via the injected provider), overriding the built-in.
        use crate::expander::VariableProvider;

        #[derive(Clone)]
        struct FixedDate;
        impl VariableProvider for FixedDate {
            fn resolve(&self, name: &str) -> Option<String> {
                match name {
                    "DATE" => Some("2026-08-05".into()),
                    "CLIPBOARD" => Some(String::new()),
                    _ => None,
                }
            }
        }

        let e = ImeEngine::with_config(
            crate::family::pinyin::PinyinWeights::default(),
            crate::family::english::EnglishWeights::default(),
            Box::new(FixedDate),
            vec![("/sig".into(), "Best regards,\nAlice\n$DATE".into())],
            crate::scoring::ScoringConfig::default(),
        );
        let mut e = e;
        for c in "#/sig".chars() { e.predict(KeyEvent::char(c)); }
        let v = e.predict(KeyEvent::space());
        assert_eq!(
            ImeView::str_field(&v.commit_text),
            "Best regards,\nAlice\n2026-08-05",
            "config snippet overrides built-in + $DATE expands"
        );
    }

    #[test]
    fn asr_without_aura_connection_shows_unavailable_in_preedit() {
        // #asr while aura is down (no buffer attached at all, or a disconnected
        // one): the preedit says the voice path is unavailable instead of
        // pretending to recognize; Space commits nothing.
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();

        // Case 1: no buffer attached at all (aura client never spawned).
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        let v0 = e.view();
        let preedit = ImeView::str_field(&v0.preedit_text);
        assert!(preedit.contains("未连接"), "preedit explains unavailability: {preedit}");
        assert!(preedit.contains("语音不可用"), "preedit mentions voice unavailable: {preedit}");
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "", "Space must not commit the explainer");
        let v = e.predict(KeyEvent::escape());
        assert!(ImeView::str_field(&v.commit_text).is_empty(), "escape cancels");

        // Case 2: buffer attached but reported disconnected.
        let buf = Arc::new(AsrBuffer::new());
        buf.push_final("陈旧语音文本"); // stale data must not masquerade as live
        e.set_asr_buffer(Arc::clone(&buf));
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        let v = e.view();
        let preedit = ImeView::str_field(&v.preedit_text);
        assert!(preedit.contains("未连接"), "disconnected buffer still explains: {preedit}");
        let v = e.predict(KeyEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "", "stale voice text never committed");
    }

    #[test]
    fn asr_reconnects_when_aura_comes_back() {
        // Connection flips don't touch the version counter — tick must still
        // rebuild when connectivity changes (disconnect → unavailable, reconnect
        // → normal recognition display).
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        e.set_asr_buffer(Arc::clone(&buf));

        // Not connected → unavailable explainer.
        for c in "#asr".chars() { e.predict(KeyEvent::char(c)); }
        assert!(ImeView::str_field(&e.view().preedit_text).contains("未连接"));

        // Voice data arrives while still disconnected (version bumps) — tick
        // rebuilds, but the display stays unavailable.
        buf.push_final("断线时的文本");
        assert!(e.magic_tick().is_some(), "tick rebuilds on voice data");
        assert!(ImeView::str_field(&e.view().preedit_text).contains("未连接"));

        // Connection restored (no new voice data — version unchanged) — tick
        // must rebuild from the connectivity flip alone.
        buf.set_connected(true);
        assert!(e.magic_tick().is_some(), "tick rebuilds on connectivity flip");
        let v = e.view();
        let preedit = ImeView::str_field(&v.preedit_text);
        assert!(!preedit.contains("未连接"), "back to normal after reconnect: {preedit}");
        assert!(preedit.contains("断线时的文本"), "stale-while-offline text now shown: {preedit}");

        // And the drop again: disconnect with no data change → unavailable.
        buf.set_connected(false);
        assert!(e.magic_tick().is_some(), "tick rebuilds on disconnect");
        let v = e.view();
        assert!(ImeView::str_field(&v.preedit_text).contains("未连接"));
    }

    #[test]
    fn emoji_tsv_load_enables_pinyin_keywords() {
        // 外部词表(CLDR 生成的拼音关键词)经 load_emoji_dict 加载后,
        // 拼音输入触发对应 emoji —— 汉字关键词无法在拼音 buffer 触发,
        // 词表必须携带拼音形式。
        let path = format!("/tmp/swift-ime-emoji-load-{}.tsv", std::process::id());
        std::fs::write(&path, "ganlan\t🥦\n").unwrap();
        let mut e = eng();
        e.load_emoji_dict(&path).unwrap();
        for c in "ganlan".chars() { e.predict(KeyEvent::char(c)); }
        assert!(e.candidates().contains(&"🥦".to_string()),
            "ganlan surfaces 🥦: {:?}", e.candidates());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emoji_family_can_be_disabled_at_runtime() {
        // dicts.emoji: false → 整个家族退出统一打分,无任何 emoji 候选。
        let mut e = eng();
        e.set_family_enabled("emoji", false);
        for c in "smile".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates_detailed();
        assert!(cands.iter().all(|d| d.family != "emoji"),
            "no emoji candidates when disabled: {cands:?}");
        // 恢复后候选回来(新引擎,避免上一段的 buffer 残留)。
        let mut e2 = eng();
        e2.set_family_enabled("emoji", true);
        for c in "smile".chars() { e2.predict(KeyEvent::char(c)); }
        assert!(e2.candidates_detailed().iter().any(|d| d.family == "emoji"),
            "emoji candidates return when re-enabled");
    }

    #[test]
    fn emoji_family_competes_in_unified_ranking() {
        // Emoji 是并列于中英文的第三家族:"smile" 输入时 😊 经统一打分
        // 出现在候选区(emoji priority 60 → exact 1.0 × 0.6 = 0.6)。
        let mut e = eng();
        for c in "smile".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        assert!(cands.contains(&"😊".to_string()), "smile surfaces 😊: {cands:?}");

        // 拼音关键词同样生效:"weixiao" → 😊。
        let mut e2 = eng();
        for c in "weixiao".chars() { e2.predict(KeyEvent::char(c)); }
        assert!(e2.candidates().contains(&"😊".to_string()), "weixiao surfaces 😊: {:?}", e2.candidates());
    }

    #[test]
    fn candidate_meta_shows_provider_and_weight_in_debug_mode() {
        // 调试模式开启时,候选槽的 meta 字段填充 [score family/source]。
        let mut e = eng();
        e.set_candidate_meta(true);
        for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
        let v = e.view();
        assert!(v.candidate_count > 0);
        let meta = ImeView::str_field(&v.candidates[0].meta);
        assert!(meta.starts_with('[') && meta.contains('/'),
            "meta shows [score family/source]: {meta:?}");
        assert!(meta.contains("pinyin"), "family in meta: {meta:?}");

        // 关闭后 meta 为空。
        let mut e2 = eng();
        e2.set_candidate_meta(false);
        for c in "nihao".chars() { e2.predict(KeyEvent::char(c)); }
        let v = e2.view();
        assert_eq!(ImeView::str_field(&v.candidates[0].meta), "",
            "debug off → no meta");
    }

    #[test]
    fn context_aware_off_skips_recency_boost() {
        // input.context_aware: false 时,recency 加成被跳过 —— 同一词的分数
        // 与冷启动(无上下文)完全一致。
        let score_of = |context_aware: bool| {
            let mut e = eng();
            e.set_context_aware(context_aware);
            // 先提交 你好(写入 recency),再查 nihao。
            for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
            e.predict(KeyEvent::space());
            for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
            e.candidates_detailed().iter().find(|c| c.text == "你好")
                .map(|c| c.score)
                .unwrap_or(0.0)
        };
        let on = score_of(true);
        let off = score_of(false);
        assert!(on > off, "context on boosts 你好 (recency): {on} vs {off}");
        // 修复后词典词(你好)不进 phrase → 关闭时无短语/上下文记忆,分数
        // 与冷启动(从未提交)完全一致 —— recency 加成确实被跳过。
        let mut e = eng();
        for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
        let cold = e.candidates_detailed().iter().find(|c| c.text == "你好")
            .map(|c| c.score).unwrap_or(0.0);
        assert!((off - cold).abs() < 1e-9,
            "context off == cold start (no phrase memory): {off} vs {cold}");
    }

    #[test]
    fn direct_space_commit_of_decomp_never_becomes_phrase() {
        // 用户场景(qingqiuti→请求提 回归):直接空格提交 decomp 选项
        // **不学**进单词本 —— 自生词的唯一入口是数字键逐字选择路径。
        // decomp 词下次输入时 Viterbi 重新组合出同样的候选,无需入本。
        let mut e = eng();
        // 多音节输入,候选含 decomp(Viterbi 造词)。
        for c in "qingqiuti".chars() { e.predict(KeyEvent::char(c)); }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c == "请求提"),
            "decomp 请求提 present: {cands:?}");
        // 直接空格提交 top(decomp)。
        e.predict(KeyEvent::space());

        // 再次输入:仍是 decomp 来源。若被学进单词本,phrase(0.70)会盖过
        // decomp(0.32)且 source 变 "phrase"(即用户截图中的
        // `[0.708 pinyin/phrase]`)—— source 检测即 bug 的直接证据。
        for c in "qingqiuti".chars() { e.predict(KeyEvent::char(c)); }
        let detailed = e.candidates_detailed();
        let req = detailed.iter().find(|d| d.text == "请求提")
            .expect("请求提 still a candidate (via decomp)");
        assert_eq!(req.source, "decomp",
            "direct space commit must NOT learn: {req:?}\n{detailed:?}");
    }

    #[test]
    fn composed_selection_joins_phrase_even_if_in_dictionary() {
        // 自生词模式:多字拼音 + 数字键逐字选择(你→好)组成的整体无条件
        // 加入单词本 —— 即使 你好 在词典里(与直接提交不同,直接提交时
        // 词典词不进 phrase,见 dictionary_words_are_not_learned)。
        let mut e = eng();
        for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
        // 数字键选 single 单字"你"(partial commit,进入自生词模式)
        let ni = e.candidates().iter().position(|c| *c == "你").expect("你 as single option");
        e.select_candidate(ni);
        // 余下 "hao":继续选"好"(full commit,所有输入字都有归属)
        let hao = e.candidates().iter().position(|c| *c == "好").expect("好");
        let v = e.select_candidate(hao);
        assert_eq!(ImeView::str_field(&v.commit_text), "你好");

        // 再查 nihao → 你好 来自 phrase(自生词无条件加入生效)。
        for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
        let detailed = e.candidates_detailed();
        assert!(detailed.iter().any(|d| d.text == "你好" && d.source == "phrase"),
            "composed 你好 must be in the phrase book: {:?}",
            detailed.iter().map(|d| (&d.text, d.source)).take(8).collect::<Vec<_>>());
    }

    #[test]
    fn english_learned_word_survives_restart() {
        // cd + Enter(强制提交 raw)→ 学成英文自生词,重启后 warm,
        // cd 作为 english/user 候选 #1(0.616,压过 emoji 前缀与中文简拼)。
        let db = format!("/tmp/swift-ime-enlearn-{}.db", std::process::id());
        let _ = std::fs::remove_file(&db);
        {
            let mut e = eng();
            e.init_store(&db);
            for c in "cd".chars() { e.predict(KeyEvent::char(c)); }
            let v = e.predict(KeyEvent::enter());
            assert_eq!(ImeView::str_field(&v.commit_text), "cd", "Enter commits raw");
        }
        {
            let mut e = eng();
            e.init_store(&db);
            for c in "cd".chars() { e.predict(KeyEvent::char(c)); }
            let detailed = e.candidates_detailed();
            let cd = detailed.iter().find(|d| d.text == "cd")
                .expect("learned cd is a candidate");
            assert_eq!(cd.family, "english");
            assert_eq!(cd.source, "user");
            assert_eq!(detailed[0].text, "cd", "learned word ranks #1");
        }
        // 中文提交不触发英文学习。
        {
            let mut e = eng();
            e.init_store(&db);
            for c in "nihao".chars() { e.predict(KeyEvent::char(c)); }
            e.predict(KeyEvent::space());
            let store = crate::store::WeightStore::open(&db).unwrap();
            let en = store.load_all_en_user();
            assert_eq!(en, vec![("cd".to_string(), 1)], "chinese commit doesn't learn: {en:?}");
        }
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn scoring_config_priorities_affect_ranking() {
        // family_priority 来自 swift-ime.yaml:英文优先级配成 0 → black 的
        // 最终分 = 0(被全局排序压到底),默认 70 时 > 0。
        let black_score = |english_priority: u32| {
            let mut scoring = crate::scoring::ScoringConfig::default();
            scoring.priorities.english = english_priority;
            let mut e = ImeEngine::with_config(
                crate::family::pinyin::PinyinWeights::default(),
                crate::family::english::EnglishWeights::default(),
                Box::new(crate::expander::DefaultProvider),
                Vec::new(),
                scoring,
            );
            for c in "black".chars() { e.predict(KeyEvent::char(c)); }
            e.candidates_detailed().iter().find(|c| c.text == "black")
                .map(|c| c.score)
                .unwrap_or(0.0)
        };
        assert!(black_score(70) > 0.0, "default english priority keeps black ranked");
        assert_eq!(black_score(0), 0.0, "priority 0 zeroes the final score");
    }

    #[test]
    fn set_variable_reaches_injected_provider() {
        // The fcitx5 frontend pushes clipboard text via set_variable → the
        // injected provider (shared with the expander) must observe it.
        use crate::expander::VariableProvider;

        #[derive(Clone)]
        struct Recording {
            values: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        }
        impl VariableProvider for Recording {
            fn resolve(&self, _name: &str) -> Option<String> { None }
            fn set(&self, name: &str, value: &str) {
                self.values.lock().unwrap().push((name.to_string(), value.to_string()));
            }
        }

        let values = Arc::new(std::sync::Mutex::new(Vec::new()));
        let e = ImeEngine::with_config(
            crate::family::pinyin::PinyinWeights::default(),
            crate::family::english::EnglishWeights::default(),
            Box::new(Recording { values: Arc::clone(&values) }),
            Vec::new(),
            crate::scoring::ScoringConfig::default(),
        );
        e.set_variable("CLIPBOARD", "剪贴板文本");
        e.set_variable("CLIPBOARD", "更新后的文本");
        assert_eq!(
            *values.lock().unwrap(),
            vec![
                ("CLIPBOARD".into(), "剪贴板文本".into()),
                ("CLIPBOARD".into(), "更新后的文本".into()),
            ]
        );
    }

    // 特殊键路由(idle 透传 / 面板开时导航 / Esc 门控)的测试在
    // router.rs —— 决策矩阵现在由状态机表统一持有。
}
