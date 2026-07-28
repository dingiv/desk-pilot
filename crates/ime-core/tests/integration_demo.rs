//! Integration test demo — full engine prediction pipeline via ImeEngine.
//! No fcitx5 required. Tests pinyin, incremental composition, context boost,
//! snippets, and English prediction.

use ime_core::engine::{ImeEngine, InputEvent};
use ime_core::ImeView;

fn commit(view: &ImeView) -> &str {
    ImeView::str_field(&view.commit_text)
}

#[test]
fn predict_nihao_top_is_hello() {
    let mut eng = ImeEngine::new();
    for c in "nihao".chars() { eng.predict(InputEvent::char(c)); }
    let cands = eng.candidates();
    assert!(cands.first().map_or(false, |c| c.contains("你好")),
        "top should be 你好, got {:?}", &cands[..5.min(cands.len())]);
}

#[test]
fn predict_xiayige() {
    let mut eng = ImeEngine::new();
    for c in "xiayige".chars() { eng.predict(InputEvent::char(c)); }
    assert!(eng.candidates().iter().any(|c| *c == "下一个"),
        "should contain 下一个");
}

#[test]
fn incremental_composition_full_flow() {
    let mut eng = ImeEngine::new();

    for c in "lizhengming".chars() { eng.predict(InputEvent::char(c)); }
    eprintln!("lizhengming: {:?}", &eng.candidates()[..10]);

    let li_idx = eng.candidates().iter().position(|c| *c == "李").expect("李 not found");
    eng.select_candidate(li_idx);

    let zheng_idx = eng.candidates().iter().position(|c| *c == "正").expect("正 not found");
    eng.select_candidate(zheng_idx);

    let ming_idx = eng.candidates().iter().position(|c| *c == "明").expect("明 not found");
    let v = eng.select_candidate(ming_idx);
    assert_eq!(commit(&v), "李正明");
}

#[test]
fn snippet_slash_greet() {
    let mut eng = ImeEngine::new();
    for c in "/greet".chars() {
        eng.predict(InputEvent::char(c));
    }
    // Completing the trigger shows a candidate; space commits it.
    let v = eng.predict(InputEvent::space());
    assert_eq!(commit(&v), "你好，我是 AI 秘书，请问有什么可以帮你的？");
}

#[test]
fn snippet_enter_commits_raw_trigger() {
    let mut eng = ImeEngine::new();
    for c in "/greet".chars() {
        eng.predict(InputEvent::char(c));
    }
    // Enter should commit the raw trigger text, not expand.
    let v = eng.predict(InputEvent::enter());
    assert_eq!(commit(&v), "/greet");
}

#[test]
fn context_boost_dalu() {
    let mut eng = ImeEngine::new();

    for c in "da".chars() { eng.predict(InputEvent::char(c)); }
    eng.predict(InputEvent::space());

    for c in "lu".chars() { eng.predict(InputEvent::char(c)); }
    let cands = eng.candidates();
    let lu_pos = cands.iter().position(|c| *c == "陆").unwrap_or(usize::MAX);
    eprintln!("With context 大: 陆@{}, 路@{}",
        lu_pos, cands.iter().position(|c| *c == "路").unwrap_or(usize::MAX));
    assert!(lu_pos != usize::MAX, "陆 should be in candidates");
}

#[test]
fn backspace_during_composition() {
    let mut eng = ImeEngine::new();
    eng.predict(InputEvent::char('n'));
    eng.predict(InputEvent::char('i'));
    assert_eq!(eng.buffer(), "ni");
    eng.predict(InputEvent::backspace());
    assert_eq!(eng.buffer(), "n");
    eng.predict(InputEvent::backspace());
    assert!(eng.buffer().is_empty());
}

#[test]
fn enter_commits_raw_pinyin() {
    let mut eng = ImeEngine::new();
    for c in "hello".chars() { eng.predict(InputEvent::char(c)); }
    assert_eq!(commit(&eng.predict(InputEvent::enter())), "hello");
}

#[test]
fn english_word_black() {
    let mut eng = ImeEngine::new();
    for c in "blac".chars() { eng.predict(InputEvent::char(c)); }
    assert!(eng.candidates().contains(&"black".to_string()),
        "black should be in candidates for 'blac'");
}

#[test]
fn multi_context_isolation() {
    let eng = ImeEngine::new();
    eng.predict_ctx(1, 'n');
    eng.predict_ctx(1, 'i');
    eng.predict_ctx(2, 'h');
    eng.predict_ctx(2, 'a');
    let v = eng.predict_ctx(1, ' ');
    assert!(commit(&v).contains("你"));
}
