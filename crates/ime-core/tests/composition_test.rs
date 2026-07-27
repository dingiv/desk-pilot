use ime_core::*;

#[test]
fn incremental_composition_lizhengming() {
    let dispatcher = Dispatcher::new(
        Matcher::new(vec![]),
        Expander::new(Box::new(expander::StaticProvider {
            date: "2026-07-23".into(),
            clipboard: String::new(),
        })),
    );
    let mut sm = state::StateMachine::new();

    // Type "lizhengming"
    for c in "lizhengming".chars() {
        dispatcher.process_key(c, &mut sm);
    }

    // Check we have candidates with both full comps and single-char options
    eprintln!("buffer={:?}", sm.buffer);
    eprintln!("candidates ({} total, {} full comps): {:?}",
        sm.candidates.len(), sm.full_comp_count, &sm.candidates[..sm.candidates.len().min(15)]);
    eprintln!("committed_text={:?}", sm.committed_text);
    eprintln!("full_comp_count={}", sm.full_comp_count);

    assert!(!sm.candidates.is_empty(), "should have candidates for lizhengming");
    assert!(sm.full_comp_count > 0, "should have full compositions");
    assert!(sm.candidates.len() > sm.full_comp_count,
        "should have single-char options beyond full comps");

    // Find "李" in the single-char options (index >= full_comp_count)
    let li_idx = sm.candidates.iter().position(|c| c == "李").expect("李 should be in candidates");
    eprintln!("李 at index {li_idx}");

    // Select "李" (partial commit)
    let view = dispatcher.select_candidate(li_idx, &mut sm);
    eprintln!("after selecting 李: buffer={:?}, committed={:?}, preedit={:?}",
        sm.buffer, sm.committed_text, ImeView::str_field(&view.preedit_text));
    assert!(sm.committed_text.contains("李"), "committed should contain 李");
    assert!(ImeView::str_field(&view.preedit_text).contains("李"), "preedit should contain 李");
    assert_eq!(sm.buffer, "zhengming", "buffer should be zhengming after committing li");
    assert!(view.commit_text[0] == 0, "partial commit should NOT commit to app");

    // Now candidates should be for "zhengming"
    eprintln!("candidates for zhengming: {:?}", &sm.candidates[..sm.candidates.len().min(10)]);

    // Find "正" in single-char options
    let zheng_idx = sm.candidates.iter().position(|c| c == "正").expect("正 should be in candidates");
    let _view2 = dispatcher.select_candidate(zheng_idx, &mut sm);
    eprintln!("after selecting 正: buffer={:?}, committed={:?}", sm.buffer, sm.committed_text);
    eprintln!("candidates for ming: {:?}", &sm.candidates[..sm.candidates.len().min(15)]);
    assert!(sm.committed_text.contains("正"), "committed should contain 正");
    assert_eq!(sm.buffer, "ming");

    // Now for "ming", select "明" (full commit since it's a single syllable)
    let ming_idx = sm.candidates.iter().position(|c| c == "明").expect("明 should be in candidates");
    let view3 = dispatcher.select_candidate(ming_idx, &mut sm);
    let commit_text = ImeView::str_field(&view3.commit_text);
    eprintln!("final commit: {:?}", commit_text);
    assert_eq!(commit_text, "李正明", "final commit should be 李正明");
    assert_eq!(sm.state, state::ComposeState::Idle);
    assert!(sm.committed_text.is_empty());
}

#[test]
fn incremental_backspace_undoes_char() {
    let dispatcher = Dispatcher::new(
        Matcher::new(vec![]),
        Expander::new(Box::new(expander::StaticProvider {
            date: "2026-07-23".into(),
            clipboard: String::new(),
        })),
    );
    let mut sm = state::StateMachine::new();

    // Type "lizheng"
    for c in "lizheng".chars() {
        dispatcher.process_key(c, &mut sm);
    }

    // Select "李" (partial commit)
    let li_idx = sm.candidates.iter().position(|c| c == "李").unwrap();
    dispatcher.select_candidate(li_idx, &mut sm);
    assert!(sm.committed_text.contains("李"));

    // Backspace should undo the committed char
    dispatcher.process_key('\x08', &mut sm);
    eprintln!("after backspace: committed={:?}, buffer={:?}", sm.committed_text, sm.buffer);
    assert!(!sm.committed_text.contains("李"), "backspace should undo 李");
    // Buffer should be "lizheng" again (approximately)
    assert!(!sm.buffer.is_empty());
}
