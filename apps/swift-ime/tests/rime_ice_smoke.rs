//! Smoke tests verifying rime-ice dictionary is loaded and effective.
//! Run with: cargo test -p swift-ime --test rime_ice_smoke -- --nocapture

use ime_core::engine::{ImeEngine, InputEvent};
use std::path::Path;

/// Find rime-ice.tsv — tries dev paths relative to workspace root.
fn rime_ice_path() -> Option<String> {
    for candidate in &[
        "apps/swift-ime/assets/dict/rime-ice.tsv",
        "assets/dict/rime-ice.tsv",
    ] {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn engine_with_rime() -> ImeEngine {
    let engine = ImeEngine::new();
    let path = rime_ice_path().expect("rime-ice.tsv not found — run from workspace root");
    let n = engine.load_dict(&path).expect("failed to load rime-ice");
    assert!(n > 500_000, "expected 500k+ entries, got {n}");
    engine
}

fn candidates_for(engine: &mut ImeEngine, input: &str) -> Vec<String> {
    for c in input.chars() {
        engine.predict(InputEvent::char(c));
    }
    engine.candidates()
}

// ── Dictionary loading ─────────────────────────────────────────────────

#[test]
fn rime_ice_loads_successfully() {
    let path = rime_ice_path().expect("rime-ice.tsv not found");
    let engine = ImeEngine::new();
    let n = engine.load_dict(&path).expect("load_dict failed");
    assert!(n > 500_000, "expected 500k+ entries, got {n}");
    eprintln!("rime-ice: {n} entries loaded");
}

// ── Common words from rime-ice ─────────────────────────────────────────

#[test]
fn common_word_shuru() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "shuru");
    assert!(cands.iter().any(|c| c == "输入"),
        "rime-ice should have 输入 for shuru, got {:?}", &cands[..10.min(cands.len())]);
}

#[test]
fn common_word_dakai() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "dakai");
    assert!(cands.iter().any(|c| c == "打开"),
        "rime-ice should have 打开 for dakai");
}

#[test]
fn common_word_sousuo() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "sousuo");
    assert!(cands.iter().any(|c| c == "搜索"),
        "rime-ice should have 搜索 for sousuo");
}

#[test]
fn common_word_shuoming() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "shuoming");
    assert!(cands.iter().any(|c| c == "说明"),
        "rime-ice should have 说明 for shuoming");
}

// ── Specific user-reported words ───────────────────────────────────────

#[test]
fn specific_qianyiyuanze() {
    let mut e = engine_with_rime();
    for c in "qianyiyuanze".chars() { e.predict(InputEvent::char(c)); }
    let cands = e.candidates();
    eprintln!("qianyiyuanze candidates (first 5): {:?}", &cands[..5.min(cands.len())]);
    assert!(!cands.is_empty(), "qianyiyuanze should have candidates");
    assert!(cands[0].contains("谦抑") || cands.iter().any(|c| c.contains("谦抑")),
        "qianyiyuanze should find 谦抑原则, got: {:?}", &cands[..5.min(cands.len())]);
}

// ── Previously problematic ─────────────────────────────────────────────

#[test]
fn previously_missing_qianyi() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "qianyi");
    eprintln!("qianyi candidates (first 20): {:?}", &cands[..20.min(cands.len())]);
    assert!(cands.contains(&"迁移".to_string()),
        "rime-ice should have 迁移 for qianyi (may be on page 2)");
}

#[test]
fn previously_missing_yichu() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "yichu");
    eprintln!("yichu candidates (first 20): {:?}", &cands[..20.min(cands.len())]);
    assert!(cands.contains(&"移除".to_string()),
        "rime-ice should have 移除 for yichu (may be on page 2)");
}

// ── Baseline: words inputx already handles ─────────────────────────────

#[test]
fn baseline_nihao_still_works() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "nihao");
    assert!(cands.iter().any(|c| c.contains("你好")),
        "nihao should still work with rime-ice");
}

#[test]
fn baseline_zhongguo_still_works() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "zhongguo");
    assert!(cands.iter().any(|c| c == "中国"),
        "zhongguo should have 中国");
}

// ── English words (should NOT come from rime-ice, but EnglishFamily) ───

#[test]
fn english_black_still_works() {
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "black");
    assert!(cands.contains(&"black".to_string()),
        "english word black should be found by EnglishFamily, not rime-ice");
}
