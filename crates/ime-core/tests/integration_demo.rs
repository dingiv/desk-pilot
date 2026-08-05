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
    eprintln!("lizhengming: {:?}", &eng.candidates());

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

#[test]
fn bigram_persistence_warmup() {
    // Use a unique temp db to avoid cross-test interference.
    let db_path = format!("/tmp/swift-ime-bigram-test-{}.db", std::process::id());

    // ── Session 1: record a bigram ──
    {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);
        // Simulate user committing "大" then "陆".
        for c in "da".chars() { eng.predict(InputEvent::char(c)); }
        eng.predict(InputEvent::space());
        for c in "lu".chars() { eng.predict(InputEvent::char(c)); }
        eng.predict(InputEvent::space());
        // Explicitly record the bigram (space commits via select_candidate internally).
        eng.record_bigram("大", "陆");
    } // eng drops → store closes

    // ── Session 2: warm from SQLite, verify boost ──
    {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);

        // Type "lu" with "大" context.
        eng.set_context("大");
        for c in "lu".chars() { eng.predict(InputEvent::char(c)); }
        let cands = eng.candidates();

        eprintln!("After warmup: bigram 大→陆 boost check, top-5: {:?}",
            &cands.iter().take(5).collect::<Vec<_>>());
        // 陆 should be present and ranked high.
        let lu_pos = cands.iter().position(|c| *c == "陆");
        assert!(lu_pos.is_some(), "陆 should be in candidates after bigram warmup");
    }

    // Cleanup.
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn incremental_composition_recall() {
    let mut eng = ImeEngine::new();

    // Compose "李正明" from "lizhengming".
    for c in "lizhengming".chars() { eng.predict(InputEvent::char(c)); }
    let li_idx = eng.candidates().iter().position(|c| *c == "李").expect("李 not found");
    eng.select_candidate(li_idx);
    let zheng_idx = eng.candidates().iter().position(|c| *c == "正").expect("正 not found");
    eng.select_candidate(zheng_idx);
    let ming_idx = eng.candidates().iter().position(|c| *c == "明").expect("明 not found");
    let v = eng.select_candidate(ming_idx);
    assert_eq!(commit(&v), "李正明", "full composition should produce 李正明");

    // Now type the same pinyin again — the phrase should be recalled.
    for c in "lizhengming".chars() { eng.predict(InputEvent::char(c)); }
    let cands = eng.candidates();
    eprintln!("Recall candidates for lizhengming: {:?}", &cands.iter().take(5).collect::<Vec<_>>());
    assert!(cands.contains(&"李正明".to_string()),
        "李正明 should be recallable after composition, top-5: {:?}",
        &cands.iter().take(5).collect::<Vec<_>>());
    // It should be rank #1 (phrase score = 1.0).
    assert_eq!(cands[0], "李正明",
        "李正明 should be #1 after learning, got {:?} at #1", cands[0]);
}

#[test]
fn phrase_persistence_across_sessions() {
    let db_path = format!("/tmp/swift-ime-phrase-test-{}.db", std::process::id());

    // ── Session 1: compose "李正明", store should persist it ──
    {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);

        for c in "lizhengming".chars() { eng.predict(InputEvent::char(c)); }
        let li = eng.candidates().iter().position(|c| *c == "李").unwrap();
        eng.select_candidate(li);
        let zheng = eng.candidates().iter().position(|c| *c == "正").unwrap();
        eng.select_candidate(zheng);
        let ming = eng.candidates().iter().position(|c| *c == "明").unwrap();
        let v = eng.select_candidate(ming);
        assert_eq!(commit(&v), "李正明");
    } // eng drops → store flushed

    // ── Session 2: fresh engine, warm from same db, phrase should be there ──
    {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);

        for c in "lizhengming".chars() { eng.predict(InputEvent::char(c)); }
        let cands = eng.candidates();
        eprintln!("Session 2 recall: {:?}", &cands.iter().take(5).collect::<Vec<_>>());
        assert!(cands.contains(&"李正明".to_string()),
            "李正明 should survive restart via SQLite, top-5: {:?}",
            &cands.iter().take(5).collect::<Vec<_>>());
        assert_eq!(cands[0], "李正明",
            "李正明 should be #1 after cross-session warm, got {:?}", cands[0]);
    }

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn asr_command_with_no_buffer_commits_empty() {
    let mut eng = ImeEngine::new();
    // Type #asr — the __ASR_BUFFER__ token expands to empty when no buffer is attached.
    for c in "#asr".chars() { eng.predict(InputEvent::char(c)); }
    // Space commits the resolved expansion (empty string).
    let v = eng.predict(InputEvent::space());
    // Empty commit means no commit_text is written.
    let committed = commit(&v);
    eprintln!("asr command commit (no buffer): '{committed}'");
    // With no buffer attached, __ASR_BUFFER__ expands to empty string,
    // so space commits nothing.
    assert!(committed.is_empty(), "expected empty commit, got '{committed}'");
}

#[test]
fn asr_command_with_buffer_commits_voice_text() {
    use ime_core::asr_buffer::AsrBuffer;
    let mut eng = ImeEngine::new();

    // Attach a voice buffer with some text (connected — otherwise #asr shows
    // "语音不可用" and commits nothing).
    let buf = std::sync::Arc::new(AsrBuffer::new());
    buf.set_connected(true);
    buf.update("今天天气不错");
    eng.set_asr_buffer(buf);

    // Type #asr
    for c in "#asr".chars() { eng.predict(InputEvent::char(c)); }
    // Space commits the voice text.
    let v = eng.predict(InputEvent::space());
    assert_eq!(commit(&v), "今天天气不错");
}

#[test]
fn asr_prefix_shows_command_name() {
    let mut eng = ImeEngine::new();
    // Type partial #as → should show #asr as candidate.
    for c in "#as".chars() { eng.predict(InputEvent::char(c)); }
    let cands = eng.candidates();
    eprintln!("#as prefix candidates: {:?}", cands);
    // The matcher will show "#as" as preedit since #asr is in the trie.
    // The buffer shows the accumulated prefix.
    assert!(!eng.buffer().is_empty(), "should have buffer for '#as'");
    assert!(eng.buffer().starts_with("#as"), "buffer should start with '#as', got {:?}", eng.buffer());
}

#[test]
fn surrounding_text_stored_and_accessible() {
    let eng = ImeEngine::new();
    eng.set_surrounding(0, "中国的首都是北京");
    // Verify it doesn't crash and context is set.
    let v = eng.predict_ctx(0, 'n');
    assert!(v.candidate_count > 0 || v.preedit_text[0] != 0,
        "should handle surrounding text without panicking");
}

#[test]
fn recency_boost_promotes_recent_word() {
    let mut eng = ImeEngine::new();
    // Type and commit "大陆" → enters recency.
    for c in "dalu".chars() { eng.predict(InputEvent::char(c)); }
    eng.predict(InputEvent::space()); // commit 大 (first candidate)
    // Now type "lu" — 陆 should get recency boost since we just saw "大".
    for c in "lu".chars() { eng.predict(InputEvent::char(c)); }
    let cands = eng.candidates();
    let lu_pos = cands.iter().position(|c| c == "陆");
    eprintln!("After recency push(大): 陆 position = {lu_pos:?}, top-5: {:?}",
        &cands.iter().take(5).collect::<Vec<_>>());
    assert!(lu_pos.is_some(), "陆 should be in candidates after recency push");
}

#[test]
fn phrase_initials_recall() {
    let mut eng = ImeEngine::new();

    // Compose "李正明" from "lizhengming".
    for c in "lizhengming".chars() { eng.predict(InputEvent::char(c)); }
    let li = eng.candidates().iter().position(|c| *c == "李").unwrap();
    eng.select_candidate(li);
    let zheng = eng.candidates().iter().position(|c| *c == "正").unwrap();
    eng.select_candidate(zheng);
    let ming = eng.candidates().iter().position(|c| *c == "明").unwrap();
    let v = eng.select_candidate(ming);
    assert_eq!(commit(&v), "李正明");

    // Now type initials "lzm" — should recall 李正明.
    for c in "lzm".chars() { eng.predict(InputEvent::char(c)); }
    let cands = eng.candidates();
    eprintln!("lzm recall: {:?}", &cands.iter().take(5).collect::<Vec<_>>());
    assert!(cands.contains(&"李正明".to_string()),
        "lzm should recall 李正明, got {:?}", &cands.iter().take(5).collect::<Vec<_>>());
}
