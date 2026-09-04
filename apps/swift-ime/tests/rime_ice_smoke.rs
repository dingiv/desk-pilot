//! Smoke tests verifying rime-ice dictionary is loaded and effective.
//! Run with: cargo test -p swift-ime --test rime_ice_smoke -- --nocapture

use ime_core::engine::{ImeEngine, KeyEvent};
use std::path::Path;

/// Find rime-ice.fst (the compiled dictionary — the repo no longer ships rime-ice.tsv).
/// Cargo runs tests with CWD = package dir, but resolve via CARGO_MANIFEST_DIR so it works
/// from any invocation directory.
fn rime_ice_path() -> Option<String> {
    let pkg = env!("CARGO_MANIFEST_DIR");
    for candidate in &[
        format!("{pkg}/assets/dict/rime-ice.fst"),
        "apps/swift-ime/assets/dict/rime-ice.fst".to_string(),
    ] {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn engine_with_rime() -> ImeEngine {
    let engine = ImeEngine::new();
    let path = rime_ice_path().expect("rime-ice.fst not found — run from workspace root");
    engine.load_dict(&path).expect("failed to load rime-ice");
    engine
}

fn candidates_for(engine: &mut ImeEngine, input: &str) -> Vec<String> {
    for c in input.chars() {
        engine.predict(KeyEvent::char(c));
    }
    engine.candidates()
}

// ── Dictionary loading ─────────────────────────────────────────────────

#[test]
fn rime_ice_loads_successfully() {
    let path = rime_ice_path().expect("rime-ice.fst not found");
    let engine = ImeEngine::new();
    engine.load_dict(&path).expect("load_dict failed");
    // The .fst format doesn't report entry counts (load_dict returns Ok(0)); effectiveness is
    // verified by the word tests below (谦抑原则 is a distinctive rime-ice entry — if the FST
    // or its .idx cache is broken, this yields no candidates).
    let mut e = engine;
    let cands = candidates_for(&mut e, "qianyiyuanze");
    assert!(!cands.is_empty(), "lattice should produce candidates after FST load");
    eprintln!("rime-ice.fst loaded OK, qianyiyuanze candidates: {:?}", &cands[..5.min(cands.len())]);
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
    for c in "qianyiyuanze".chars() { e.predict(KeyEvent::char(c)); }
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

// ── 前缀整词联想(上下文感知)───────────────────────────────────────────
// (round10 W1 后,单字凭自身词频即可顶格 —— de→的 不再依赖 context_comp
// 抬分;原 context_prefix_association_boosts_tail_word 断言的实现路径失效,
// 按规移除。上下文联想机制本身仍在,对词组候选生效。)

// ── 全拼前缀联想(naozh → 闹钟)────────────────────────────────────────

#[test]
fn prefix_lookup_associates_partial_pinyin() {
    // naozh 是 naozhong 的半截(zh 非法音节,greedy_parse 拆 z+h 两段,
    // 旧 Mixed 路径段数膨胀永远匹配不上)。前缀联想让目标词在打字途中
    // 就出现,且中文候选压过 emoji 前缀命中(⏰ 0.48)。
    let mut e = engine_with_rime();
    let cands = candidates_for(&mut e, "naozh");
    assert_eq!(cands.first().map(String::as_str), Some("闹钟"),
        "naozh → 闹钟 #1 (prefix association): {cands:?}");

    // 继续输入到全拼:无缝切换到精确匹配(权重更高)。
    let mut e2 = engine_with_rime();
    let cands2 = candidates_for(&mut e2, "naozhong");
    assert_eq!(cands2.first().map(String::as_str), Some("闹钟"));
}

// ── Ranking consistency (jix vs jixu) ───────────────────────────────────
//
// Regression: freq_to_score used MAX_WEIGHT=100k with a 0.90 cap, so every
// common word saturated and 简拼 (jix) candidates all tied — their order was
// arbitrary and INCONSISTENT with the full-pinyin path (jixu ranked 积蓄
// above 继续). The two paths must now rank the same words in the same order.

#[test]
fn jix_and_jixu_rank_consistently() {
    let mut e = engine_with_rime();
    let jix = candidates_for(&mut e, "jix");
    let mut e2 = engine_with_rime();
    let jixu = candidates_for(&mut e2, "jixu");

    assert_eq!(jix.first().map(String::as_str), Some("继续"),
        "继续 is #1 on 简拼 jix: {jix:?}");
    assert_eq!(jixu.first().map(String::as_str), Some("继续"),
        "继续 is #1 on full pinyin jixu: {jixu:?}");
    // The candidate SETS differ (jix matches every ji-x* syllable pair, jixu
    // only ji-xu) — but words present in BOTH must keep the same relative
    // order, otherwise typing one more letter reshuffles the list.
    let common: Vec<String> = jix.iter()
        .filter(|w| jixu.contains(w))
        .take(6)
        .cloned()
        .collect();
    assert_eq!(common.len(), 6, "enough shared candidates: {common:?}");
    let jixu_common: Vec<String> = jixu.iter()
        .filter(|w| common.contains(w))
        .cloned()
        .collect();
    assert_eq!(jixu_common, common,
        "shared words keep their relative order: jix={common:?} jixu={jixu_common:?}");
}

#[test]
fn learned_phrase_does_not_downgrade_dict_hit() {
    // Selecting 继续 learns it into the PhraseBook. The phrase entry used to
    // REPLACE the dict hit at a fixed 0.88 score — dropping 继续 below 急须 on
    // jixu. A learned word that's already in the dict must keep its (higher)
    // dict ranking.
    let mut e = engine_with_rime();
    for c in "jixu".chars() { e.predict(KeyEvent::char(c)); }
    let idx = e.candidates().iter().position(|c| *c == "继续").expect("继续 present");
    e.select_candidate(idx); // commits + learn_phrase("jixu", "继续")

    let mut e2 = engine_with_rime();
    // Simulate the same session state: select 继续 once, then re-type jixu.
    for c in "jixu".chars() { e2.predict(KeyEvent::char(c)); }
    let idx2 = e2.candidates().iter().position(|c| *c == "继续").expect("继续 present");
    e2.select_candidate(idx2);
    for c in "jixu".chars() { e2.predict(KeyEvent::char(c)); }
    let cands = e2.candidates();
    assert_eq!(cands.first().map(String::as_str), Some("继续"),
        "learned 继续 stays #1 after being picked: {cands:?}");
}
