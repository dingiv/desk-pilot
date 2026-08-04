//! Mock frontend — standalone evaluation tool for testing the IME engine.
//!
//! Provides batch evaluation, interactive mode, single-input testing, and
//! debug commands (`#asr`, `#flush`, `#submit`, etc.).
//!
//! The caller (main.rs) parses CLI args and passes them as a [`MockConfig`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use ime_core::asr_buffer::AsrBuffer;
use ime_core::engine::{ImeEngine, InputEvent};
use ime_core::ImeView;

// ── Config ─────────────────────────────────────────────────────────────

/// All configuration for the mock frontend, passed from main.rs.
#[derive(Debug, Clone)]
pub struct MockConfig {
    pub cases: Option<String>,
    pub input: Option<String>,
    pub top_n: usize,
    pub verbose: bool,
    pub config: Option<String>,
    pub asr_text: Option<String>,
    pub commit: bool,
    pub async_wait: u64,
    pub connect_aura: bool,
    pub aura_addr: Option<String>,
    pub surrounding: Option<String>,
    pub en_user_dict: Option<String>,
    pub en_dicts: Vec<String>,
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig {
            cases: None, input: None,
            top_n: 160, verbose: true,
            config: None, asr_text: None,
            commit: false, async_wait: 0,
            connect_aura: false, aura_addr: None,
            surrounding: None,
            en_user_dict: None, en_dicts: Vec::new(),
        }
    }
}

// ── Entry points ──────────────────────────────────────────────────────

/// Run batch evaluation against a test cases file.
pub fn run_cases_mode(cfg: &MockConfig, cases_path: &str) {
    crate::logger::init_default();
    let (mut engine, _, _) = build_engine(cfg);
    run_cases(&mut engine, cases_path, cfg.verbose);
}

/// Run single-input mode (show candidates or commit).
pub fn run_input_mode(cfg: &MockConfig) {
    crate::logger::init_default();
    let (mut engine, asr, _) = build_engine(cfg);
    let input = cfg.input.as_deref().unwrap_or("");
    if cfg.async_wait > 0 { wait_for_voice(&asr, cfg.async_wait); }
    if cfg.commit {
        show_commit(&mut engine, input, cfg.verbose);
    } else {
        show_candidates_with_async(&mut engine, &asr, input, cfg.top_n, cfg.verbose, cfg.async_wait);
    }
}

/// Build the IME engine with all config applied. Returns the engine, the shared voice buffer,
/// and (if connecting to aura) a connectivity handle for the frontend to display.
pub fn build_engine(cfg: &MockConfig) -> (ImeEngine, Arc<AsrBuffer>, Option<crate::bridge::AuraConnHandle>) {
    let sw_cfg = if let Some(ref path) = cfg.config {
        match std::fs::read_to_string(path) {
            Ok(yaml) => match serde_yaml::from_str::<crate::config::SwiftImeConfig>(&yaml) {
                Ok(c) => { crate::ime_log!("loaded config from {path}"); c }
                Err(e) => { crate::ime_log!("config parse error: {e}, using defaults"); crate::config::SwiftImeConfig::default() }
            },
            Err(e) => { crate::ime_log!("config read error: {e}, using defaults"); crate::config::SwiftImeConfig::default() }
        }
    } else {
        crate::config::SwiftImeConfig::load()
    };

    let weights = sw_cfg.weights.pinyin.to_engine();
    let eng_weights = ime_core::family::english::EnglishWeights {
        exact: sw_cfg.weights.english.exact,
        prefix_ratio: sw_cfg.weights.english.prefix_ratio,
        user_boost: sw_cfg.weights.english.user_boost,
    };
    let engine = ImeEngine::with_config(weights, sw_cfg.weights.family_priority.english, eng_weights);

    if sw_cfg.dicts.rime_ice {
        let loader = shared::loader!("assets");
        if let Some(p) = loader.resolve("DICT::rime-ice.fst") {
            match engine.load_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded {n} dict entries from {}", p.display()),
                Err(e) => crate::ime_log!("dict load error: {e}"),
            }
        }
    }

    let asr_buffer = Arc::new(AsrBuffer::new());
    if let Some(ref text) = cfg.asr_text {
        asr_buffer.update(text);
        crate::ime_log!("asr mock text: {text}");
    }
    engine.set_asr_buffer(Arc::clone(&asr_buffer));

    let aura_status = if cfg.connect_aura || cfg.aura_addr.is_some() {
        let addr = cfg.aura_addr.as_deref().unwrap_or("127.0.0.1:9091");
        crate::ime_log!("connecting to aura: {addr}");
        Some(crate::bridge::spawn_aura_client(Arc::clone(&asr_buffer), Some(addr)))
    } else {
        None
    };

    let home = std::env::var("HOME").unwrap_or_default();
    engine.init_store(&format!("{home}/.desk-pilot/swift-ime.db"));
    (engine, asr_buffer, aura_status)
}

// ── Async wait ─────────────────────────────────────────────────────────

fn wait_for_voice(asr_buffer: &AsrBuffer, timeout_secs: u64) {
    if !asr_buffer.snapshot().is_empty() { return; }
    let timeout = Duration::from_secs(timeout_secs);
    let start = Instant::now();
    eprintln!("⏳ waiting for aura SSE (up to {timeout_secs}s)...");
    while asr_buffer.snapshot().is_empty() && start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(200));
    }
    if asr_buffer.snapshot().is_empty() {
        eprintln!("⏰ timeout — no voice data from aura");
    } else {
        eprintln!("📥 voice data received");
    }
}

// ── Candidate display ──────────────────────────────────────────────────

fn show_candidates_with_async(
    engine: &mut ImeEngine, asr_buffer: &AsrBuffer, input: &str,
    top_n: usize, verbose: bool, async_wait_secs: u64,
) -> Vec<String> {
    for c in input.chars() { engine.predict(InputEvent::char(c)); }

    let cands = engine.candidates();
    let is_preview = cands.first().map_or(false, |c| c.ends_with("..."));

    if is_preview && async_wait_secs > 0 {
        let timeout = Duration::from_secs(async_wait_secs);
        let start = Instant::now();
        eprintln!("⏳ async wait (up to {async_wait_secs}s) — polling for voice data...");
        while asr_buffer.snapshot().is_empty() && start.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(200));
        }
        let elapsed = start.elapsed();
        if asr_buffer.snapshot().is_empty() {
            eprintln!("⏰ timeout after {:.1}s — no voice data arrived", elapsed.as_secs_f64());
        } else {
            eprintln!("📥 voice data arrived after {:.1}s", elapsed.as_secs_f64());
        }
        engine.predict(InputEvent::enter());
        for c in input.chars() { engine.predict(InputEvent::char(c)); }
    }

    let candidates = engine.candidates();
    if verbose {
        let detailed = engine.candidates_detailed();
        print_candidates_verbose(input, &detailed, top_n);
    } else {
        let n = candidates.len().min(top_n);
        for i in 0..n {
            let marker = if candidates[i].ends_with("...") { "⚡" } else { "" };
            println!("{}{}", marker, &candidates[i]);
        }
        if candidates.is_empty() { println!("(no candidates)"); }
    }

    engine.predict(InputEvent::enter());
    candidates
}

fn print_candidates_verbose(input: &str, detailed: &[ime_core::family::RankedCandidate], top_n: usize) {
    let n = detailed.len().min(top_n);
    println!();
    println!("── {input} ──");
    for i in 0..n {
        let d = &detailed[i];
        let marker = if i == 0 { "★" } else { " " };
        let kind = if d.text.ends_with("...") { "⚡preview" } else { "" };
        println!("  [{:>2}] {} {:<24} {:>5.3}  {}/{}  {}",
            i + 1, marker, d.text, d.score, d.family, d.source, kind);
    }
    if detailed.is_empty() { println!("  (no candidates)"); }
    else if detailed.len() > n { println!("  ... {} more", detailed.len() - n); }
}

// ── Commit display ─────────────────────────────────────────────────────

fn show_commit(engine: &mut ImeEngine, input: &str, verbose: bool) {
    let mut last_view = ImeView::empty();
    for c in input.chars() { last_view = engine.predict(InputEvent::char(c)); }

    if verbose {
        let detailed = engine.candidates_detailed();
        println!();
        println!("── {input} (pre-commit) ──");
        for (i, d) in detailed.iter().enumerate() {
            let marker = if i == 0 { "★" } else { " " };
            let kind = if d.text.ends_with("...") { "⚡preview" } else { "" };
            println!("  [{:>2}] {} {:<24}  {:>5.3}  {}/{}  {}",
                i + 1, marker, d.text, d.score, d.family, d.source, kind);
        }
        if detailed.is_empty() { println!("  (no candidates before commit)"); }
    }

    let commit_view = engine.predict(InputEvent::space());
    let committed = ImeView::str_field(&commit_view.commit_text);
    println!();
    if committed.is_empty() {
        let was_preview = engine.candidates().first().map_or(false, |c| c.ends_with("..."));
        if was_preview { println!("── {input} (commit) → (empty — preview, no voice data yet)"); }
        else { println!("── {input} (commit) → (empty — no voice data or unknown command)"); }
    } else {
        println!("── {input} (commit) → \"{committed}\"");
    }

    if verbose {
        let preedit = ImeView::str_field(&last_view.preedit_text);
        let aux = ImeView::str_field(&last_view.aux_up);
        if !preedit.is_empty() { println!("   preedit: \"{preedit}\""); }
        if !aux.is_empty() && aux != preedit { println!("   aux:     \"{aux}\""); }
    }
}

// ── Batch evaluation ───────────────────────────────────────────────────

struct TestCase { pinyin: String, expected: String }

fn parse_cases(path: &str) -> Vec<TestCase> {
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read cases file '{path}': {e}"));
    let mut cases = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            cases.push(TestCase { pinyin: parts[0].to_string(), expected: parts[1].to_string() });
        }
    }
    cases
}

fn run_cases(engine: &mut ImeEngine, path: &str, verbose: bool) {
    let cases = parse_cases(path);
    let mut total = 0u32; let mut top1 = 0u32; let mut top3 = 0u32; let mut top10 = 0u32;
    if verbose { println!("Evaluating {} test cases...\n", cases.len()); }

    for tc in &cases {
        total += 1;
        for c in tc.pinyin.chars() { engine.predict(InputEvent::char(c)); }
        let cands = engine.candidates();
        let pos = cands.iter().position(|c| c == &tc.expected);
        if pos == Some(0) { top1 += 1; }
        if pos.map_or(false, |p| p < 3) { top3 += 1; }
        if pos.map_or(false, |p| p < 10) { top10 += 1; }
        if verbose || pos.is_none() {
            let pos_str = pos.map_or("-".to_string(), |p| format!("#{}", p + 1));
            let icon = match pos { Some(0) => "✅", Some(_) => "⚠️", None => "❌" };
            println!("{icon} {:<20} → {:<12}  ({})", tc.pinyin, tc.expected, pos_str);
            if pos.is_none() && verbose {
                let top = &cands[..cands.len().min(5)];
                println!("     got: {:?}", top);
            }
        }
        engine.predict(InputEvent::enter());
    }

    println!();
    println!("═══════════════════════════════════");
    println!("  Total:     {:>5}", total);
    println!("  Top-1:     {:>5}  ({:.1}%)", top1, top1 as f64 / total as f64 * 100.0);
    println!("  Top-3:     {:>5}  ({:.1}%)", top3, top3 as f64 / total as f64 * 100.0);
    println!("  Top-10:    {:>5}  ({:.1}%)", top10, top10 as f64 / total as f64 * 100.0);
    println!("═══════════════════════════════════");
}

