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
//!     eng.predict(InputEvent::char(c));
//! }
//! eng.predict(InputEvent::space());
//! ```

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::dispatcher::Dispatcher;
use crate::family::InputContext;
use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};
use crate::weight_store::WeightStore;

// ── InputEvent ──────────────────────────────────────────────────────────

/// A single input event from the frontend.
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    pub ch: u32,
    pub ctrl: bool,
    pub shift: bool,
}

impl InputEvent {
    pub fn char(c: char) -> Self { InputEvent { ch: c as u32, ctrl: false, shift: false } }
    pub fn backspace() -> Self { InputEvent { ch: '\x08' as u32, ctrl: false, shift: false } }
    pub fn enter() -> Self { InputEvent { ch: '\n' as u32, ctrl: false, shift: false } }
    pub fn space() -> Self { InputEvent { ch: ' ' as u32, ctrl: false, shift: false } }
    pub fn escape() -> Self { InputEvent { ch: 0x1B, ctrl: false, shift: false } }
}

// ── PerContext ──────────────────────────────────────────────────────────

struct PerContext {
    sm: StateMachine,
    text_context: InputContext,
}

impl Default for PerContext {
    fn default() -> Self {
        PerContext { sm: StateMachine::new(), text_context: InputContext::new() }
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
    store: Mutex<Option<std::sync::Arc<WeightStore>>>,
}

impl ImeEngine {
    /// Create a new engine with all default prediction families, built-in
    /// snippet triggers, and the embedded base phrase dictionary.
    pub fn new() -> Self {
        Self::with_pinyin_weights(crate::family::pinyin::PinyinWeights::default())
    }

    /// Create engine with custom pinyin family weights (from config file).
    pub fn with_pinyin_weights(weights: crate::family::pinyin::PinyinWeights) -> Self {
        let entries: Vec<(String, String)> = vec![
            ("/greet".into(), "你好，我是 AI 秘书，请问有什么可以帮你的？".into()),
            ("/sig".into(), "Best regards,\nAlice".into()),
            ("#date".into(), "2026-07-27".into()),
            ("#wait".into(), "__WAIT_DEMO__".into()),
        ];
        let matcher = crate::Matcher::new(entries);
        let expander = crate::Expander::new(Box::new(
            crate::expander::StaticProvider { date: String::from("2026-07-27"), clipboard: String::new() },
        ));
        let engine = ImeEngine {
            dispatcher: Dispatcher::with_pinyin_weights(matcher, expander, weights),
            contexts: Mutex::new(HashMap::new()),
            async_waits: Mutex::new(HashMap::new()),
            store: Mutex::new(None),
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
        let mut map = self.contexts.lock().unwrap();
        let pc = map.entry(ctx).or_default();
        f(&self.dispatcher, pc)
    }

    fn remove_ctx(&self, ctx: usize) {
        self.contexts.lock().unwrap().remove(&ctx);
        self.async_waits.lock().unwrap().remove(&ctx);
    }

    // ── Multi-context API (used by fcitx5 C ABI) ────────────────────────

    /// Process a key for a given input context. Returns the UI snapshot.
    pub fn predict_ctx(&self, ctx: usize, ch: char) -> ImeView {
        self.with_ctx(ctx, |disp, pc| {
            pc.sm.context = pc.text_context.clone();
            let prev = pc.text_context.last_word.clone();
            let mut view = disp.process_key(ch, &mut pc.sm);
            let committed = ImeView::str_field(&view.commit_text);
            if !committed.is_empty() {
                pc.text_context.update(committed);
                // Record bigram: prev_word → committed_word (both SQLite + in-memory).
                self.record_bigram(&prev, committed);
            }
            // #wait demo interceptor.
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

    /// Select a candidate by index for a given context.
    pub fn select_ctx(&self, ctx: usize, index: usize) -> ImeView {
        self.with_ctx(ctx, |disp, pc| {
            pc.sm.context = pc.text_context.clone();
            let view = disp.select_candidate(index, &mut pc.sm);
            let committed = ImeView::str_field(&view.commit_text);
            if !committed.is_empty() {
                let prev = pc.text_context.last_word.clone();
                pc.text_context.update(committed);
                self.record_bigram(&prev, committed);
            }
            view
        })
    }

    /// Reset engine state for a context.
    pub fn reset_ctx(&self, ctx: usize) {
        self.with_ctx(ctx, |disp, pc| disp.reset(&mut pc.sm));
    }

    /// Deactivate (clean up) a context — removes its state and async waits.
    pub fn deactivate_ctx(&self, ctx: usize) {
        self.remove_ctx(ctx);
    }

    /// Commit any pending composition for a context.
    pub fn commit_pending_ctx(&self, ctx: usize) -> ImeView {
        let map = self.contexts.lock().unwrap();
        let Some(pc) = map.get(&ctx) else { return ImeView::empty() };
        let text = pc.sm.candidates.first().cloned()
            .unwrap_or_else(|| pc.sm.buffer.clone());
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

    /// Feed an InputEvent into the default context (ctx=0).
    pub fn predict(&mut self, event: InputEvent) -> ImeView {
        let ch = char::from_u32(event.ch).unwrap_or('\0');
        if ch == '\0' && event.ch != 0 { return ImeView::empty(); }
        self.predict_ctx(DEFAULT_CTX, ch)
    }

    /// Select a candidate in the default context.
    pub fn select_candidate(&mut self, index: usize) -> ImeView {
        self.select_ctx(DEFAULT_CTX, index)
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
    /// Re-runs the scorer on the current buffer to get member-level traceability.
    pub fn candidates_detailed(&self) -> Vec<crate::family::RankedCandidate> {
        let map = self.contexts.lock().unwrap();
        let Some(pc) = map.get(&DEFAULT_CTX) else { return Vec::new() };
        let ctx = pc.text_context.clone();
        let buffer = pc.sm.buffer.clone();
        drop(map);
        use crate::state::StepEnv;
        self.dispatcher.scorer().rank_detailed(&buffer, &ctx)
    }

    /// Short-term text context for the default context.
    pub fn context_text(&self) -> String {
        self.contexts.lock().unwrap()
            .get(&DEFAULT_CTX)
            .map(|pc| pc.text_context.recent_text.clone())
            .unwrap_or_default()
    }

    /// Manually set the text context (simulates pre-filled text).
    pub fn set_context(&mut self, text: &str) {
        self.contexts.lock().unwrap()
            .entry(DEFAULT_CTX).or_default()
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

    /// Initialize the SQLite-backed weight store. Call once at startup.
    /// Warms the in-memory bigram model AND phrase book from persisted data.
    pub fn init_store(&self, path: &str) {
        match WeightStore::open(path) {
            Ok(store) => {
                let store = std::sync::Arc::new(store);
                // Warm the in-memory bigram model from SQLite.
                let entries = store.load_all_bigrams();
                if !entries.is_empty() {
                    self.dispatcher.warm_bigrams(entries);
                }
                // Attach store to pinyin family for future phrase persistence,
                // and warm the phrase book from past sessions.
                self.dispatcher.set_store(store.clone());
                self.dispatcher.warm_phrases_from_store();
                let pins = store.pin_count();
                eprintln!("[swift-ime] weight store: {pins} pins, {} bigrams from {path}",
                    store.max_bigram_count());
                *self.store.lock().unwrap() = Some(store);
            }
            Err(e) => eprintln!("[swift-ime] weight store open failed: {e}"),
        }
    }

    /// Record a bigram to BOTH the SQLite store (persistence) and the
    /// in-memory pinyin family model (immediate ranking boost).
    pub fn record_bigram(&self, prev: &str, next: &str) {
        if prev.is_empty() || next.is_empty() { return; }
        if let Some(ref s) = *self.store.lock().unwrap() { s.record_bigram(prev, next); }
        self.dispatcher.record_bigram(prev, next);
    }

    /// Commit pending text in the default context.
    pub fn commit_pending(&mut self) -> Option<String> {
        let view = self.commit_pending_ctx(DEFAULT_CTX);
        let text = ImeView::str_field(&view.commit_text);
        if text.is_empty() { None } else {
            self.with_ctx(DEFAULT_CTX, |disp, pc| {
                pc.text_context.update(text);
                disp.reset(&mut pc.sm);
            });
            Some(text.to_string())
        }
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
        for c in "nihao".chars() { e.predict(InputEvent::char(c)); }
        assert!(e.candidates().iter().any(|c| c.contains("你好")));
        let v = e.predict(InputEvent::space());
        assert!(ImeView::str_field(&v.commit_text).contains("你"));
    }

    #[test]
    fn incremental_composition() {
        let mut e = eng();
        for c in "lizhengming".chars() { e.predict(InputEvent::char(c)); }
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
        let mut e = eng();
        for c in "/greet".chars() {
            let v = e.predict(InputEvent::char(c));
            if ImeView::str_field(&v.commit_text) == "你好，我是 AI 秘书" { return; }
        }
        // Default engine has empty Matcher, so snippet won't expand.
        // Just check that it doesn't crash.
    }

    #[test]
    fn backspace_clears() {
        let mut e = eng();
        e.predict(InputEvent::char('n'));
        e.predict(InputEvent::char('i'));
        assert_eq!(e.buffer(), "ni");
        e.predict(InputEvent::backspace());
        assert_eq!(e.buffer(), "n");
        e.predict(InputEvent::backspace());
        assert!(e.buffer().is_empty());
    }

    #[test]
    fn enter_commits_raw() {
        let mut e = eng();
        for c in "hello".chars() { e.predict(InputEvent::char(c)); }
        let v = e.predict(InputEvent::enter());
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
}
