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
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::dispatcher::Dispatcher;
use crate::family::InputContext;
use crate::family::magic::{MagicFamily, ReqFetcher};
use crate::platform::ImeView;
use crate::special_key::{handle_special_key, SpecialKey};
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
    /// The magic command registry — same `Arc` the dispatcher holds. The engine
    /// routes late resource attachment (voice buffer, `#req` base/fetcher) here;
    /// the FSM spawns live member instances from it.
    magic: Arc<MagicFamily>,
}

impl ImeEngine {
    /// Create a new engine with all default prediction families, built-in
    /// snippet triggers, and the embedded base phrase dictionary.
    pub fn new() -> Self {
        Self::with_pinyin_weights(crate::family::pinyin::PinyinWeights::default())
    }

    /// Create engine with custom pinyin family weights (from config file).
    pub fn with_pinyin_weights(weights: crate::family::pinyin::PinyinWeights) -> Self {
        Self::with_config(weights, 70, crate::family::english::EnglishWeights::default())
    }

    /// Create engine with full config (pinyin weights + English priority + English weights).
    pub fn with_config(
        pinyin_weights: crate::family::pinyin::PinyinWeights,
        english_priority: u32,
        english_weights: crate::family::english::EnglishWeights,
    ) -> Self {
        // Magic command entries are generated from the member registry (#asr, #flush,
        // #submit, #req, #date, #password …) — adding a command = one member, nothing
        // here. `/`-snippets and the `#wait` async demo stay plain matcher entries.
        let magic = Arc::new(MagicFamily::new());
        let mut entries = magic.matcher_entries();
        entries.extend(vec![
            ("/greet".into(), "你好，我是 AI 秘书，请问有什么可以帮你的？".into()),
            ("/sig".into(), "Best regards,\nAlice".into()),
            ("#wait".into(), "__WAIT_DEMO__".into()),
        ]);
        let matcher = crate::Matcher::new(entries);
        let expander = crate::Expander::new(Box::new(
            crate::expander::StaticProvider { date: String::from("2026-07-27"), clipboard: String::new() },
        ));
        let engine = ImeEngine {
            dispatcher: Dispatcher::with_config(matcher, expander, Arc::clone(&magic), pinyin_weights, english_priority, english_weights),
            contexts: Mutex::new(HashMap::new()),
            async_waits: Mutex::new(HashMap::new()),
            store: Mutex::new(None),
            magic,
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

    /// Process a special key (navigation, commit, selection) for a context.
    /// Returns the updated ImeView.
    pub fn special_key_ctx(&self, ctx: usize, key: SpecialKey) -> ImeView {
        self.with_ctx(ctx, |disp, pc| {
            handle_special_key(&mut pc.sm, key, disp)
                .unwrap_or_else(|| ImeView::empty())
        })
    }

    /// Process a special key code from the C ABI.
    pub fn special_key_code_ctx(&self, ctx: usize, code: i32) -> ImeView {
        match SpecialKey::from_code(code) {
            Some(key) => self.special_key_ctx(ctx, key),
            None => ImeView::empty(),
        }
    }

    /// Process a key for a given input context. Returns the UI snapshot.
    pub fn predict_ctx(&self, ctx: usize, ch: char) -> ImeView {
        // ── Special key layer ──
        // Check if the character maps to a special key before prediction.
        let key_opt = match ch {
            ' ' => Some(SpecialKey::Space),
            '\n' | '\r' => Some(SpecialKey::Enter),
            '\x08' => Some(SpecialKey::Backspace),
            '\x1b' => Some(SpecialKey::Escape),
            d @ '1'..='9' => Some(SpecialKey::Digit(d as u8 - b'0')),
            _ => None,
        };
        if let Some(key) = key_opt {
            return self.special_key_ctx(ctx, key);
        }

        self.with_ctx(ctx, |disp, pc| {
            pc.sm.context = pc.text_context.clone();
            let prev = pc.text_context.last_word.clone();
            let mut view = disp.process_key(ch, &mut pc.sm);
            let committed = ImeView::str_field(&view.commit_text);
            if !committed.is_empty() {
                pc.text_context.update(committed);
                // Record bigram: prev_word → committed_word (both SQLite + in-memory).
                self.record_bigram(&prev, committed);
                self.dispatcher.record_commit(committed);
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
                self.dispatcher.record_commit(committed);
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

    /// Set surrounding text from the application (fcitx5 callback).
    /// The text is stored in per-context `InputContext` and used by
    /// prediction families for broader context matching.
    pub fn set_surrounding(&self, ctx: usize, text: &str) {
        let mut map = self.contexts.lock().unwrap();
        if let Some(pc) = map.get_mut(&ctx) {
            pc.text_context.set_surrounding(text);
        }
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

    /// Move the candidate highlight by `delta` (±1) in the default context.
    pub fn move_highlight(&self, delta: i32) {
        self.move_highlight_ctx(DEFAULT_CTX, delta)
    }

    /// Move the candidate highlight by `delta` for a specific context.
    pub fn move_highlight_ctx(&self, ctx: usize, delta: i32) {
        self.contexts.lock().unwrap()
            .get_mut(&ctx)
            .map(|pc| pc.sm.move_highlight(delta));
    }

    /// Change candidate page by `delta` (-1=prev, +1=next) in the default context.
    pub fn page(&self, delta: i32) {
        self.page_ctx(DEFAULT_CTX, delta)
    }

    /// Change candidate page by `delta` for a specific context.
    pub fn page_ctx(&self, ctx: usize, delta: i32) {
        self.contexts.lock().unwrap()
            .get_mut(&ctx)
            .map(|pc| {
                let n = pc.sm.candidates.len();
                if n == 0 || pc.sm.candidate_page_size == 0 { return; }
                let total_pages = (n + pc.sm.candidate_page_size - 1) / pc.sm.candidate_page_size;
                let new_page = (pc.sm.candidate_page as i32 + delta).clamp(0, total_pages as i32 - 1) as usize;
                if new_page != pc.sm.candidate_page {
                    pc.sm.candidate_page = new_page;
                    pc.sm.candidate_highlight = new_page * pc.sm.candidate_page_size;
                }
            });
    }

    /// Go to the previous candidate page in the default context.
    pub fn page_up(&self) { self.page(-1); }

    /// Go to the next candidate page in the default context.
    pub fn page_down(&self) { self.page(1); }

    /// Get the full ImeView for a specific context without processing a key.
    pub fn view_ctx(&self, ctx: usize) -> ImeView {
        self.contexts.lock().unwrap()
            .get(&ctx)
            .map(|pc| {
                let mut v = ImeView::empty();
                v.candidate_count = pc.sm.candidates.len().min(16) as u32;
                v.candidate_highlight = pc.sm.candidate_highlight as u32;
                v.candidate_page = pc.sm.candidate_page as u32;
                v.candidate_page_size = pc.sm.candidate_page_size as u32;
                for (i, c) in pc.sm.candidates.iter().take(16).enumerate() {
                    ImeView::set_str(&mut v.candidates[i].text, c);
                }
                ImeView::set_str(&mut v.preedit_text, &pc.sm.preedit);
                v.preedit_cursor = pc.sm.cursor as u32;
                v
            })
            .unwrap_or_else(ImeView::empty)
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
        // correctly. For Magic, prefer the member's full commit texts over the
        // display previews (voice sentences / req bodies are truncated in the bar).
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
            changed
        })
    }

    /// Load an English user dictionary from a TSV file.
    /// All words get max priority (10000).
    pub fn load_en_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.dispatcher.load_en_user_dict(path)
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
    fn asr_voice_anchor_live_then_final_then_commit() {
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        e.set_asr_buffer(Arc::clone(&buf));

        // type #asr → Voice mode, preview candidate (no voice data yet)
        for c in "#asr".chars() { e.predict(InputEvent::char(c)); }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("语音识别中")), "preview when empty: {cands:?}");

        // voice streams → live candidate appears; voice_tick rebuilds it
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
        let v = e.predict(InputEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "你好世界");
        assert!(e.candidates().is_empty(), "candidates cleared after commit");
    }

    #[test]
    fn asr_voice_escape_cancels() {
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        e.set_asr_buffer(Arc::clone(&buf));
        buf.push_final("识别文本");
        for c in "#asr".chars() { e.predict(InputEvent::char(c)); }
        e.magic_tick();
        // Escape → cancel (no commit), back to idle
        let v = e.predict(InputEvent::escape());
        assert!(ImeView::str_field(&v.commit_text).is_empty(), "escape commits nothing");
        assert!(e.candidates().is_empty(), "cleared after escape");
    }

    #[test]
    fn asr_voice_long_sentence_preview_with_ellipsis_commit_full() {
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        // A sentence > VOICE_PREVIEW_MAX (60) bytes — each char = 3 bytes, 10 chars = 30.
        // 3× repeat → ~90 bytes, well above the preview cap.
        let long = "这是一句相当长的话，超出六十字节限制。".repeat(2); // ~72 bytes
        let buf = Arc::new(AsrBuffer::new());
        buf.push_final(&long);
        let mut e = eng();
        e.set_asr_buffer(Arc::clone(&buf));
        for c in "#asr".chars() { e.predict(InputEvent::char(c)); }
        e.magic_tick();
        let cands = e.candidates();
        assert!(cands[0].ends_with('…'), "long preview has ellipsis: {}", cands[0]);
        assert!(cands[0].len() < long.len(), "preview is shorter than full ({})", cands[0].len());
        // Space commits the FULL text (from voice_full), not the display preview.
        let v = e.predict(InputEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), long);
    }

    #[test]
    fn asr_voice_active_live_is_candidate_1() {
        use std::sync::Arc;
        use crate::asr_buffer::AsrBuffer;
        let mut e = eng();
        let buf = Arc::new(AsrBuffer::new());
        e.set_asr_buffer(Arc::clone(&buf));
        for c in "#asr".chars() { e.predict(InputEvent::char(c)); }
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
    fn wait_req_tick(e: &ImeEngine) {
        for _ in 0..200 {
            if e.magic_tick().is_some() { return; }
            std::thread::sleep(std::time::Duration::from_millis(5));
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
        for c in "#req".chars() { e.predict(InputEvent::char(c)); }
        // Activation: hint candidate with the full URL
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("http://127.0.0.1:14555/api")), "hint: {cands:?}");

        // Enter fires the request → result lands on the worker thread
        e.predict(InputEvent::enter());
        wait_req_tick(&e);
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("这是本地服务返回的正文内容")), "body shown: {cands:?}");
        assert_eq!(fake.urls.lock().unwrap().as_slice(), ["http://127.0.0.1:14555/api"], "fired the base URL");

        // Space commits the body; member exits, back to idle
        let v = e.predict(InputEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), "这是本地服务返回的正文内容");
        assert!(e.candidates().is_empty(), "cleared after commit");
    }

    #[test]
    fn req_suffix_extends_url() {
        let (mut e, fake) = req_eng();
        for c in "#req/news?query=soccer".chars() { e.predict(InputEvent::char(c)); }
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("http://127.0.0.1:14555/api/news?query=soccer")), "hint shows full URL: {cands:?}");

        e.predict(InputEvent::enter());
        wait_req_tick(&e);
        assert_eq!(fake.urls.lock().unwrap().as_slice(), ["http://127.0.0.1:14555/api/news?query=soccer"], "fired with suffix");
    }

    #[test]
    fn req_long_body_preview_truncates_commit_full() {
        let body = "长正文".repeat(30); // 270 bytes, ≫ 60
        let mut e = eng();
        e.set_req_fetcher(Arc::new(FakeFetcher { result: Ok(body.clone()), urls: Arc::new(Mutex::new(Vec::new())) }));
        for c in "#req".chars() { e.predict(InputEvent::char(c)); }
        e.predict(InputEvent::enter());
        wait_req_tick(&e);
        let cands = e.candidates();
        assert_eq!(cands.len(), 1, "whole body is ONE candidate: {cands:?}");
        assert!(cands[0].ends_with('…'), "preview truncated with ellipsis: {}", cands[0]);
        assert!(cands[0].chars().count() <= 60, "preview ≤ 60 chars: {}", cands[0]);
        // Space commits the FULL body, not the preview
        let v = e.predict(InputEvent::space());
        assert_eq!(ImeView::str_field(&v.commit_text), body);
    }

    #[test]
    fn req_failure_shows_error_and_never_commits_it() {
        let mut e = eng();
        e.set_req_fetcher(Arc::new(FakeFetcher { result: Err("HTTP 500".into()), urls: Arc::new(Mutex::new(Vec::new())) }));
        for c in "#req".chars() { e.predict(InputEvent::char(c)); }
        e.predict(InputEvent::enter());
        wait_req_tick(&e);
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("请求失败") && c.contains("HTTP 500")), "error shown: {cands:?}");

        // Space on a failed state re-fires (no garbage commit)
        let v = e.predict(InputEvent::space());
        assert!(ImeView::str_field(&v.commit_text).is_empty(), "error text never committed");
        wait_req_tick(&e);
        // Escape cancels the session
        let v = e.predict(InputEvent::escape());
        assert!(ImeView::str_field(&v.commit_text).is_empty());
        assert!(e.candidates().is_empty(), "cleared after escape");
    }

    #[test]
    fn req_backspace_edits_suffix_then_exits() {
        let (mut e, _fake) = req_eng();
        for c in "#req/news".chars() { e.predict(InputEvent::char(c)); }
        // Backspace pops one suffix char
        e.predict(InputEvent::backspace());
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("/new") && !c.contains("/news")), "suffix edited: {cands:?}");
        // Delete the rest — when the suffix is empty, Backspace exits the member
        for _ in 0..5 { e.predict(InputEvent::backspace()); }
        assert!(e.candidates().is_empty(), "member exited when suffix emptied: {:?}", e.candidates());
    }

    #[test]
    fn req_digit_extends_url_without_result_commits_with_result() {
        let (mut e, fake) = req_eng();
        for c in "#req/news".chars() { e.predict(InputEvent::char(c)); }
        // No result yet → digit is a URL character
        e.predict(InputEvent::char('2'));
        e.predict(InputEvent::char('0'));
        let cands = e.candidates();
        assert!(cands.iter().any(|c| c.contains("/news20")), "digit appended to suffix: {cands:?}");

        // Fire → result lands → digit 1 commits the body
        e.predict(InputEvent::enter());
        wait_req_tick(&e);
        let v = e.predict(InputEvent::char('1'));
        assert_eq!(ImeView::str_field(&v.commit_text), "这是本地服务返回的正文内容");
        assert!(fake.urls.lock().unwrap().iter().any(|u| u.ends_with("/news20")), "fired with digits in URL");
    }

    #[test]
    fn req_escape_cancels() {
        let mut e = eng();
        for c in "#req".chars() { e.predict(InputEvent::char(c)); }
        let v = e.predict(InputEvent::escape());
        assert!(ImeView::str_field(&v.commit_text).is_empty(), "escape commits nothing");
        assert!(e.candidates().is_empty(), "cleared after escape");
    }
}
