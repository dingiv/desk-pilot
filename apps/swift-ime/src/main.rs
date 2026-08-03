//! swift-ime — DeskPilot IME evaluation tool.
//!
//! ```bash
//! # Interactive
//! echo "jishi" | cargo run --bin swift-ime --
//!
//! # Single word
//! cargo run --bin swift-ime -- --input jishi --top-n 10 --verbose
//!
//! # Magic commands
//! cargo run --bin swift-ime -- --input "#date" --verbose
//! cargo run --bin swift-ime -- --input "#asr" --asr-text "今天天气不错" --commit
//! cargo run --bin swift-ime -- --input "#password" --verbose
//!
//! # Async #asr test (waits for voice data with timeout)
//! cargo run --bin swift-ime -- --input "#asr" --async-wait 5
//!
//! # Batch against expected results
//! cargo run --bin swift-ime -- --cases assets/testcase/tc_regular.txt --verbose
//! ```

use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use ime_core::asr_buffer::AsrBuffer;
use ime_core::engine::{ImeEngine, InputEvent};
use ime_core::ImeView;

#[derive(Parser)]
#[command(name = "swift-ime", about = "DeskPilot IME evaluation tool")]
struct Args {
    /// Test cases file: `pinyin expected_word` per line.
    #[arg(long)]
    cases: Option<String>,

    /// Top N candidates to display.
    #[arg(long, default_value = "160")]
    top_n: usize,

    /// Show detailed output (source, score, family).
    #[arg(long, default_value = "true")]
    verbose: bool,

    /// Single pinyin to evaluate.
    #[arg(long)]
    input: Option<String>,

    /// Override config file path (default: swift-ime.yaml or ~/.desk-pilot/swift-ime.yaml).
    #[arg(long)]
    config: Option<String>,

    /// Mock voice input text for #asr testing (skips the aura daemon SSE connection).
    #[arg(long)]
    asr_text: Option<String>,

    /// Commit mode: after typing the input, press Space and show the committed text
    /// (tests the full trigger→expand→commit pipeline for magic commands and snippets).
    #[arg(long, default_value = "false")]
    commit: bool,

    /// Async wait timeout in seconds. When testing #asr without --asr-text, the tool
    /// waits up to this many seconds for voice data to arrive before showing the
    /// preview result. Default 0 = no wait (shows preview immediately).
    #[arg(long, default_value = "0")]
    async_wait: u64,

    /// Connect to the real aura daemon SSE stream (default: http://127.0.0.1:9091).
    /// Spawns a background thread that reads `/api/stream` and populates the voice
    /// buffer with live calibrated text. Use with `--async-wait` to test #asr.
    #[arg(long, default_value = "false")]
    connect_aura: bool,

    /// Override aura daemon address (default: 127.0.0.1:9091). Implies --connect-aura.
    #[arg(long)]
    aura_addr: Option<String>,

    /// Mock surrounding text (simulates fcitx5 surroundingTextCallback).
    /// The text is loaded into InputContext before processing the input,
    /// so context-aware prediction can use it for bigram matching.
    #[arg(long)]
    surrounding: Option<String>,
}

fn main() {
    let args = Args::parse();
    swift_ime::logger::init_default();
    swift_ime::ime_log!("swift-ime starting");
    let (mut engine, asr_buffer) = make_engine(&args);

    // Set mock surrounding text if provided.
    if let Some(ref text) = args.surrounding {
        engine.set_surrounding(0, text);
        swift_ime::ime_log!("surrounding text: {text}");
    }

    if let Some(ref cases_path) = args.cases {
        run_cases(&mut engine, cases_path, &args);
    } else if let Some(ref input) = args.input {
        // If connecting to real aura, wait for the SSE thread to seed the buffer.
        if args.async_wait > 0 {
            wait_for_voice(&asr_buffer, args.async_wait);
        }
        if args.commit {
            show_commit(&mut engine, input, args.verbose);
        } else {
            show_candidates_with_async(&mut engine, &asr_buffer, input, args.top_n, args.verbose, args.async_wait);
        }
    } else {
        interactive(&mut engine, &asr_buffer, args.top_n, args.verbose, args.asr_text.as_deref(), args.async_wait);
    }
}

fn make_engine(args: &Args) -> (ImeEngine, Arc<AsrBuffer>) {
    // Load config — CLI override > FileLoader > defaults.
    let cfg = if let Some(ref path) = args.config {
        match std::fs::read_to_string(path) {
            Ok(yaml) => {
                match serde_yaml::from_str::<swift_ime::config::SwiftImeConfig>(&yaml) {
                    Ok(c) => { swift_ime::ime_log!("loaded config from {path}"); c }
                    Err(e) => { swift_ime::ime_log!("config parse error: {e}, using defaults"); swift_ime::config::SwiftImeConfig::default() }
                }
            }
            Err(e) => { swift_ime::ime_log!("config read error: {e}, using defaults"); swift_ime::config::SwiftImeConfig::default() }
        }
    } else {
        swift_ime::config::SwiftImeConfig::load()
    };

    // Create engine with config-driven weights.
    let weights = cfg.weights.pinyin.to_engine();
    let engine = ImeEngine::with_pinyin_weights(weights);

    // Load rime-ice FST if enabled.
    if cfg.dicts.rime_ice {
        let loader = shared::loader!("assets");
        if let Some(p) = loader.resolve("DICT::rime-ice.fst") {
            match engine.load_dict(&p.to_string_lossy()) {
                Ok(n) => swift_ime::ime_log!("loaded {n} dict entries from {}", p.display()),
                Err(e) => swift_ime::ime_log!("dict load error: {e}"),
            }
        } else {
            swift_ime::ime_log!("rime-ice.fst not found. Run: ./scripts/fetch_dict.sh && cargo run --bin build_dict -- assets/dict/rime-ice.tsv assets/dict/rime-ice.fst");
        }
    }

    // ── Voice input: mock buffer or real SSE ──
    let asr_buffer = Arc::new(AsrBuffer::new());
    if let Some(ref text) = args.asr_text {
        asr_buffer.update(text);
        swift_ime::ime_log!("asr mock text: {text}");
    }
    engine.set_asr_buffer(Arc::clone(&asr_buffer));

    // Connect to real aura daemon if requested.
    if args.connect_aura || args.aura_addr.is_some() {
        let addr = args.aura_addr.as_deref().unwrap_or("127.0.0.1:9091");
        swift_ime::ime_log!("connecting to aura SSE: {addr}");
        swift_ime::bridge::spawn_aura_sse(Arc::clone(&asr_buffer), Some(addr));
    }

    // FIXME: 硬编码路径
    let home = std::env::var("HOME").unwrap_or_default();
    engine.init_store(&format!("{home}/.desk-pilot/swift-ime.db"));
    (engine, asr_buffer)
}

// ── Async wait helper ──────────────────────────────────────────────────

/// Poll the AsrBuffer until data arrives or timeout. Used when connecting
/// to the real aura daemon to give the SSE thread time to seed the buffer
/// from `/results`.
fn wait_for_voice(asr_buffer: &AsrBuffer, timeout_secs: u64) {
    if !asr_buffer.snapshot().is_empty() {
        return; // already seeded
    }
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

/// Show candidates, optionally waiting for async data (voice buffer) before
/// displaying. When `async_wait_secs > 0` and the top candidate is a preview
/// placeholder (ends with `...`), the tool polls the ASR buffer every 200ms
/// until data arrives or the timeout expires.
///
/// Returns the displayed candidates (before engine reset) for tracking.
fn show_candidates_with_async(
    engine: &mut ImeEngine,
    asr_buffer: &AsrBuffer,
    input: &str,
    top_n: usize,
    verbose: bool,
    async_wait_secs: u64,
) -> Vec<String> {
    // First pass: type the input, see if it's a preview.
    for c in input.chars() {
        engine.predict(InputEvent::char(c));
    }

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

        // Reset engine and re-type to get the fresh result.
        engine.predict(InputEvent::enter());
        for c in input.chars() {
            engine.predict(InputEvent::char(c));
        }
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
        if candidates.is_empty() {
            println!("(no candidates)");
        }
    }

    // Reset for next input.
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
    if detailed.is_empty() {
        println!("  (no candidates)");
    } else if detailed.len() > n {
        println!("  ... {} more", detailed.len() - n);
    }
}

// ── Commit display ─────────────────────────────────────────────────────

/// Type `input` character by character, then press Space to commit.
/// Shows the full trigger→expand→commit pipeline result.
fn show_commit(engine: &mut ImeEngine, input: &str, verbose: bool) {
    let mut last_view = ImeView::empty();

    for c in input.chars() {
        last_view = engine.predict(InputEvent::char(c));
    }

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
        if detailed.is_empty() {
            println!("  (no candidates before commit)");
        }
    }

    // Press Space to commit.
    let commit_view = engine.predict(InputEvent::space());
    let committed = ImeView::str_field(&commit_view.commit_text);

    println!();
    if committed.is_empty() {
        let cands = engine.candidates();
        let was_preview = cands.first().map_or(false, |c| c.ends_with("..."));
        if was_preview {
            println!("── {input} (commit) → (empty — preview, no voice data yet)");
        } else {
            println!("── {input} (commit) → (empty — no voice data or unknown command)");
        }
    } else {
        println!("── {input} (commit) → \"{committed}\"");
    }

    // Also show preedit/aux for debugging.
    if verbose {
        let preedit = ImeView::str_field(&last_view.preedit_text);
        let aux = ImeView::str_field(&last_view.aux_up);
        if !preedit.is_empty() {
            println!("   preedit: \"{preedit}\"");
        }
        if !aux.is_empty() && aux != preedit {
            println!("   aux:     \"{aux}\"");
        }
    }
}

// ── Batch evaluation ───────────────────────────────────────────────────

struct TestCase {
    pinyin: String,
    expected: String,
}

fn parse_cases(path: &str) -> Vec<TestCase> {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read cases file '{path}': {e}"));
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

fn run_cases(engine: &mut ImeEngine, path: &str, args: &Args) {
    let cases = parse_cases(path);
    let mut total = 0u32;
    let mut top1 = 0u32;
    let mut top3 = 0u32;
    let mut top10 = 0u32;

    if args.verbose {
        println!("Evaluating {} test cases...\n", cases.len());
    }

    for tc in &cases {
        total += 1;
        for c in tc.pinyin.chars() {
            engine.predict(InputEvent::char(c));
        }
        let cands = engine.candidates();
        let pos = cands.iter().position(|c| c == &tc.expected);

        if pos == Some(0) { top1 += 1; }
        if pos.map_or(false, |p| p < 3) { top3 += 1; }
        if pos.map_or(false, |p| p < 10) { top10 += 1; }

        let hit = pos.is_some();
        if args.verbose || !hit {
            let pos_str = pos.map_or("-".to_string(), |p| format!("#{}", p + 1));
            let icon = match pos { Some(0) => "✅", Some(_) => "⚠️", None => "❌" };
            println!("{icon} {:<20} → {:<12}  ({})",
                tc.pinyin, tc.expected, pos_str);
            if !hit && args.verbose {
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

// ── Interactive mode ───────────────────────────────────────────────────

fn interactive(
    engine: &mut ImeEngine,
    asr_buffer: &AsrBuffer,
    top_n: usize,
    verbose: bool,
    asr_text: Option<&str>,
    async_wait_secs: u64,
) {
    let stdin = io::stdin();
    let mut line = String::new();
    // Track the last input type for #submit context.
    let mut last_candidates: Vec<String> = Vec::new();
    let mut last_input: String = String::new();
    let mut last_was_voice: bool = false; // true if last input was #asr or #flush

    eprintln!("swift-ime eval — type pinyin, Ctrl-D to exit.");
    if let Some(text) = asr_text {
        eprintln!("ASR mock text: \"{text}\"");
    }
    eprintln!();
    eprintln!("Magic:  #date  #asr  #flush  #submit  #password");
    eprintln!("Snippet: /greet  /sig");
    eprintln!();
    eprintln!("#asr   → trigger voice fetch, show candidate (preview if waiting)");
    eprintln!("#flush → refresh from asr_buffer, show in candidate list");
    eprintln!("#submit→ show first candidate as committed (voice or pinyin)");
    eprintln!();
    eprintln!("Tip: append \" +space\" to test commit, e.g. \"nihao +space\"");
    eprintln!("     type \"async:<N>\" to simulate delayed voice input");
    eprintln!();
    loop {
        line.clear();
        print!("> ");
        io::stdout().flush().unwrap();
        if stdin.lock().read_line(&mut line).unwrap() == 0 { break; }
        let input = line.trim();
        if input.is_empty() { continue; }

        // ── async:<N> — simulate delayed voice data ──
        if let Some(delay_str) = input.strip_prefix("async:") {
            if let Ok(delay_secs) = delay_str.trim().parse::<u64>() {
                eprintln!("⏳ simulating async voice input in {delay_secs}s...");
                std::thread::sleep(Duration::from_secs(delay_secs));
                asr_buffer.update(&format!("模拟语音输入 (延迟 {}s)", delay_secs));
                eprintln!("📥 voice buffer updated — use #flush to see it");
                continue;
            }
        }

        // ── #submit: show first candidate as committed result ──
        // Context-aware: if last input was #asr/#flush, shows voice buffer.
        // If last input was pinyin, shows first candidate of that pinyin.
        if input == "#submit" {
            println!();
            if last_was_voice {
                let voice = asr_buffer.snapshot();
                if !voice.is_empty() {
                    println!("── #submit (voice) → \"{voice}\"");
                } else {
                    println!("── #submit (voice) → (empty — no voice data yet)");
                }
            } else if let Some(first) = last_candidates.first() {
                println!("── #submit (\"{last_input}\") → \"{first}\"");
            } else {
                // Fallback: check voice buffer anyway.
                let voice = asr_buffer.snapshot();
                if !voice.is_empty() {
                    println!("── #submit (voice) → \"{voice}\"");
                } else {
                    println!("── #submit → (nothing to commit)");
                }
            }
            continue;
        }

        // ── #flush: go through engine → matcher→expander → AsrBuffer ──
        if input == "#flush" {
            show_candidates_with_async(engine, asr_buffer, input, top_n, verbose, async_wait_secs);
            continue;
        }

        // Detect: if input ends with " +space", test commit flow.
        if let Some(prefix) = input.strip_suffix(" +space") {
            show_commit(engine, prefix, verbose);
            continue;
        }

        // Normal input (pinyin, #asr, #date, snippets, etc.)
        let cands = show_candidates_with_async(engine, asr_buffer, input, top_n, verbose, async_wait_secs);

        // Track for #submit context.
        if input == "#asr" || input == "#flush" {
            last_was_voice = true;
            last_input = input.to_string();
        } else if !input.starts_with('#') && !input.starts_with('/') {
            last_was_voice = false;
            last_input = input.to_string();
            last_candidates = cands;
        }
    }
}
