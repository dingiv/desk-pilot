//! swift-ime — DeskPilot IME evaluation tool.
//!
//! ```bash
//! # Interactive
//! echo "jishi" | cargo run --bin swift-ime --
//!
//! # Single word
//! cargo run --bin swift-ime -- --input jishi --top-n 10 --verbose
//!
//! # Batch against expected results
//! cargo run --bin swift-ime -- --cases assets/testcase/tc_regular.txt --verbose
//! ```

use std::io::{self, BufRead, Write};

use clap::Parser;
use ime_core::engine::{ImeEngine, InputEvent};

#[derive(Parser)]
#[command(name = "swift-ime", about = "DeskPilot IME evaluation tool")]
struct Args {
    /// Test cases file: `pinyin expected_word` per line.
    #[arg(long)]
    cases: Option<String>,

    /// Top N candidates to display.
    #[arg(long, default_value = "160")]
    top_n: usize,

    /// Show detailed output.
    #[arg(long, default_value = "true")]
    verbose: bool,

    /// Single pinyin to evaluate.
    #[arg(long)]
    input: Option<String>,

    /// Override config file path (default: swift-ime.yaml or ~/.desk-pilot/swift-ime.yaml).
    #[arg(long)]
    config: Option<String>,
}

fn main() {
    let args = Args::parse();
    swift_ime::logger::init_default();
    swift_ime::ime_log!("swift-ime starting");
    let mut engine = make_engine(&args);

    if let Some(ref cases_path) = args.cases {
        run_cases(&mut engine, cases_path, &args);
    } else if let Some(ref input) = args.input {
        show_candidates(&mut engine, input, args.top_n, args.verbose);
    } else {
        interactive(&mut engine, args.top_n, args.verbose);
    }
}

fn make_engine(args: &Args) -> ImeEngine {
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
            swift_ime::ime_log!("rime-ice.fst not found. Run: cargo run --bin build_dict -- assets/dict/rime-ice.tsv assets/dict/rime-ice.fst");
        }
    }

    // FIXME: 硬编码路径
    let home = std::env::var("HOME").unwrap_or_default();
    engine.init_store(&format!("{home}/.desk-pilot/swift-ime.db"));
    engine
}

// ── Candidate display ──────────────────────────────────────────────────

fn show_candidates(engine: &mut ImeEngine, input: &str, top_n: usize, verbose: bool) {
    for c in input.chars() {
        engine.predict(InputEvent::char(c));
    }

    if verbose {
        // Use detailed candidates to show source traceability.
        let detailed = engine.candidates_detailed();
        let n = detailed.len().min(top_n);
        println!();
        println!("── {input} ──");
        for i in 0..n {
            let d = &detailed[i];
            let marker = if i == 0 { "★" } else { " " };
            println!("  [{:>2}] {} {:<16}  {:>5.3}  {}/{}",
                i + 1, marker, d.text, d.score, d.family, d.source);
        }
        if detailed.len() > n {
            println!("  ... {} more", detailed.len() - n);
        }
    } else {
        let cands = engine.candidates();
        let n = cands.len().min(top_n);
        for i in 0..n {
            println!("{}", &cands[i]);
        }
    }
    // Reset for next input.
    engine.predict(InputEvent::enter());
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

fn interactive(engine: &mut ImeEngine, top_n: usize, verbose: bool) {
    let stdin = io::stdin();
    let mut line = String::new();
    eprintln!("swift-ime eval — type pinyin, Ctrl-D to exit.");
    loop {
        line.clear();
        print!("> ");
        io::stdout().flush().unwrap();
        if stdin.lock().read_line(&mut line).unwrap() == 0 { break; }
        let input = line.trim();
        if input.is_empty() { continue; }
        show_candidates(engine, input, top_n, verbose);
    }
}
