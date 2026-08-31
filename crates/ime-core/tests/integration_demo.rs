//! Integration test demo — full engine prediction pipeline via ImeEngine.
//! No fcitx5 required. Tests pinyin, incremental composition, context boost,
//! snippets, and English prediction.

use ime_core::engine::{ImeEngine, KeyEvent};
use ime_core::ImeView;

fn commit(view: &ImeView) -> &str {
    ImeView::str_field(&view.commit_text)
}

/// 起一个"健康"的 mock aura: `/health` 回 200,`/api/asr_stream` 回 200 并
/// **保持连接**(不回 body),让 `subscribe_events_owned` 的流活着、不触发
/// voice server 的"断联即放弃"。测试结束后进程退出,线程随之丢弃。
fn mock_aura() -> String {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { break };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut req = [0u8; 4096];
            let _ = s.read(&mut req);
            if req.windows(4).any(|w| w == b"asr_") {
                // SSE 长连接:写 header 后 hold(不回 body,连接不关)。
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n",
                );
                let _ = s.flush();
                std::thread::sleep(Duration::from_secs(30));
            } else {
                // /health 等 → 200 close。
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = s.flush();
            }
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// 用 `voice_aura_base` 构造引擎(而非默认 127.0.0.1:9091)—— 让 `#asr` 的
/// Attach 探针打到 mock,`is_connected` 保持 true,测试确定不竞态。
fn eng_with_aura(base: &str) -> ImeEngine {
    ImeEngine::with_config(
        ime_core::family::pinyin::PinyinWeights::default(),
        ime_core::family::english::EnglishWeights::default(),
        Box::new(ime_core::family::magic::expander::DefaultProvider),
        Vec::new(),
        ime_core::family::scoring::ScoringConfig::default(),
        std::sync::Arc::new(ime_core::frontend::NoopFrontend::default()),
        base.to_string(),
        ime_core::io_thread::DEFAULT_IDLE_TIMEOUT_SECS,
        Vec::new(),
        7,
    )
}

#[test]
fn predict_nihao_top_is_hello() {
    let mut eng = ImeEngine::new();
    for c in "nihao".chars() {
        eng.predict(KeyEvent::char(c));
    }
    let cands = eng.candidates();
    assert!(
        cands.first().is_some_and(|c| c.contains("你好")),
        "top should be 你好, got {:?}",
        &cands[..5.min(cands.len())]
    );
}

#[test]
fn predict_xiayige() {
    let mut eng = ImeEngine::new();
    for c in "xiayige".chars() {
        eng.predict(KeyEvent::char(c));
    }
    assert!(
        eng.candidates().iter().any(|c| *c == "下一个"),
        "should contain 下一个"
    );
}

#[test]
fn incremental_composition_full_flow() {
    let mut eng = ImeEngine::new();

    for c in "lizhengming".chars() {
        eng.predict(KeyEvent::char(c));
    }
    eprintln!("lizhengming: {:?}", eng.candidates());

    let li_idx = eng
        .candidates()
        .iter()
        .position(|c| *c == "李")
        .expect("李 not found");
    eng.select_candidate(li_idx);

    let zheng_idx = eng
        .candidates()
        .iter()
        .position(|c| *c == "正")
        .expect("正 not found");
    eng.select_candidate(zheng_idx);

    let ming_idx = eng
        .candidates()
        .iter()
        .position(|c| *c == "明")
        .expect("明 not found");
    let v = eng.select_candidate(ming_idx);
    assert_eq!(commit(&v), "李正明");
}

#[test]
fn snippet_slash_greet() {
    let mut eng = ImeEngine::new();
    for c in "#/greet".chars() {
        eng.predict(KeyEvent::char(c));
    }
    // Completing the trigger shows a candidate; space commits it.
    let v = eng.predict(KeyEvent::space());
    assert_eq!(commit(&v), "你好，我是 AI 秘书，请问有什么可以帮你的？");
}

#[test]
fn snippet_enter_commits_raw_trigger() {
    let mut eng = ImeEngine::new();
    for c in "#/greet".chars() {
        eng.predict(KeyEvent::char(c));
    }
    // Enter should commit the raw trigger text, not expand.
    let v = eng.predict(KeyEvent::enter());
    assert_eq!(commit(&v), "#/greet");
}

#[test]
fn context_boost_dalu() {
    let mut eng = ImeEngine::new();

    for c in "da".chars() {
        eng.predict(KeyEvent::char(c));
    }
    eng.predict(KeyEvent::space());

    for c in "lu".chars() {
        eng.predict(KeyEvent::char(c));
    }
    let cands = eng.candidates();
    let lu_pos = cands.iter().position(|c| *c == "陆").unwrap_or(usize::MAX);
    eprintln!(
        "With context 大: 陆@{}, 路@{}",
        lu_pos,
        cands.iter().position(|c| *c == "路").unwrap_or(usize::MAX)
    );
    assert!(lu_pos != usize::MAX, "陆 should be in candidates");
}

#[test]
fn backspace_during_composition() {
    let mut eng = ImeEngine::new();
    eng.predict(KeyEvent::char('n'));
    eng.predict(KeyEvent::char('i'));
    assert_eq!(eng.buffer(), "ni");
    eng.predict(KeyEvent::backspace());
    assert_eq!(eng.buffer(), "n");
    eng.predict(KeyEvent::backspace());
    assert!(eng.buffer().is_empty());
}

#[test]
fn enter_commits_raw_pinyin() {
    let mut eng = ImeEngine::new();
    for c in "hello".chars() {
        eng.predict(KeyEvent::char(c));
    }
    assert_eq!(commit(&eng.predict(KeyEvent::enter())), "hello");
}

#[test]
fn english_word_black() {
    let mut eng = ImeEngine::new();
    for c in "blac".chars() {
        eng.predict(KeyEvent::char(c));
    }
    assert!(
        eng.candidates().contains(&"black".to_string()),
        "black should be in candidates for 'blac'"
    );
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
fn recency_persistence_across_sessions() {
    // The recency ring (recently committed words → position-based boost) must
    // survive restarts: session 1 commits 你好, session 2 (fresh engine +
    // init_store) gives 你好 a higher score than a never-warmed engine does.
    let db_path = format!("/tmp/swift-ime-recency-test-{}.db", std::process::id());

    // ── Session 1: commit 你好 → record_commit → save_recency ──
    {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);
        for c in "nihao".chars() {
            eng.predict(KeyEvent::char(c));
        }
        eng.predict(KeyEvent::space()); // commits 你好 (top candidate)
    } // eng drops → store closes

    // ── Session 2: warm the ring from SQLite, verify the boost ──
    let warm_score = {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);
        for c in "nihao".chars() {
            eng.predict(KeyEvent::char(c));
        }
        let detailed = eng.candidates_detailed();
        detailed
            .iter()
            .find(|c| c.text == "你好")
            .map(|c| c.score)
            .expect("你好 should be a candidate")
    };

    // Baseline: a fresh engine with NO store has no recency boost.
    let cold_score = {
        let mut eng = ImeEngine::new();
        for c in "nihao".chars() {
            eng.predict(KeyEvent::char(c));
        }
        let detailed = eng.candidates_detailed();
        detailed
            .iter()
            .find(|c| c.text == "你好")
            .map(|c| c.score)
            .expect("你好 should be a candidate")
    };

    assert!(
        warm_score > cold_score,
        "recency boost restored from store: warm={warm_score:.3} cold={cold_score:.3}"
    );

    // The persisted table is also directly readable (word + last-used ms).
    let store = ime_core::store::WeightStore::open(&db_path).unwrap();
    let ring = store.load_recency();
    assert_eq!(
        ring.first().map(|(w, _)| w.as_str()),
        Some("你好"),
        "ring: {ring:?}"
    );
    assert!(ring[0].1 > 0, "timestamp persisted: {ring:?}");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn l0_picks_persist_across_sessions() {
    // The inputx-pinyin L0 user model (3 picks → auto-pin) must survive
    // restarts: session 1 picks 你好 three times, session 2 (fresh engine +
    // init_store) restores it — 你好 ranks #1 and the L0 row exists in SQLite.
    let db_path = format!("/tmp/swift-ime-l0-test-{}.db", std::process::id());

    // ── Session 1: pick 你好 3× (record_pick → save_l0) ──
    {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);
        for _ in 0..3 {
            for c in "nihao".chars() {
                eng.predict(KeyEvent::char(c));
            }
            let idx = eng
                .candidates()
                .iter()
                .position(|c| *c == "你好")
                .expect("你好 should be a candidate");
            eng.select_candidate(idx);
        }
    }

    // ── Session 2: warm L0 from SQLite, verify the pin wins ──
    {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);
        for c in "nihao".chars() {
            eng.predict(KeyEvent::char(c));
        }
        let cands = eng.candidates();
        assert_eq!(
            cands.first().map(String::as_str),
            Some("你好"),
            "3 picks in the previous session pin 你好 to #1: {:?}",
            cands.iter().take(5).collect::<Vec<_>>()
        );
    }

    // The L0 model JSON is persisted.
    let store = ime_core::store::WeightStore::open(&db_path).unwrap();
    let l0 = store.load_l0().expect("L0 model persisted");
    assert!(
        l0.contains("你好") || l0.contains("\"nihao\""),
        "L0 JSON mentions the pick: {l0}"
    );

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn incremental_composition_recall() {
    let mut eng = ImeEngine::new();

    // Compose "李正明" from "lizhengming".
    for c in "lizhengming".chars() {
        eng.predict(KeyEvent::char(c));
    }
    let li_idx = eng
        .candidates()
        .iter()
        .position(|c| *c == "李")
        .expect("李 not found");
    eng.select_candidate(li_idx);
    let zheng_idx = eng
        .candidates()
        .iter()
        .position(|c| *c == "正")
        .expect("正 not found");
    eng.select_candidate(zheng_idx);
    let ming_idx = eng
        .candidates()
        .iter()
        .position(|c| *c == "明")
        .expect("明 not found");
    let v = eng.select_candidate(ming_idx);
    assert_eq!(
        commit(&v),
        "李正明",
        "full composition should produce 李正明"
    );

    // Now type the same pinyin again — the phrase should be recalled.
    for c in "lizhengming".chars() {
        eng.predict(KeyEvent::char(c));
    }
    let cands = eng.candidates();
    eprintln!(
        "Recall candidates for lizhengming: {:?}",
        cands.iter().take(5).collect::<Vec<_>>()
    );
    assert!(
        cands.contains(&"李正明".to_string()),
        "李正明 should be recallable after composition, top-5: {:?}",
        cands.iter().take(5).collect::<Vec<_>>()
    );
    // It should be rank #1 (phrase score = 1.0).
    assert_eq!(
        cands[0], "李正明",
        "李正明 should be #1 after learning, got {:?} at #1",
        cands[0]
    );
}

#[test]
fn phrase_persistence_across_sessions() {
    let db_path = format!("/tmp/swift-ime-phrase-test-{}.db", std::process::id());

    // ── Session 1: compose "李正明", store should persist it ──
    {
        let mut eng = ImeEngine::new();
        eng.init_store(&db_path);

        for c in "lizhengming".chars() {
            eng.predict(KeyEvent::char(c));
        }
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

        for c in "lizhengming".chars() {
            eng.predict(KeyEvent::char(c));
        }
        let cands = eng.candidates();
        eprintln!(
            "Session 2 recall: {:?}",
            cands.iter().take(5).collect::<Vec<_>>()
        );
        assert!(
            cands.contains(&"李正明".to_string()),
            "李正明 should survive restart via SQLite, top-5: {:?}",
            cands.iter().take(5).collect::<Vec<_>>()
        );
        assert_eq!(
            cands[0], "李正明",
            "李正明 should be #1 after cross-session warm, got {:?}",
            cands[0]
        );
    }

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn asr_command_with_no_buffer_commits_empty() {
    let mut eng = ImeEngine::new();
    // Type #asr — the __ASR_BUFFER__ token expands to empty when no buffer is attached.
    for c in "#asr".chars() {
        eng.predict(KeyEvent::char(c));
    }
    // Space commits the resolved expansion (empty string).
    let v = eng.predict(KeyEvent::space());
    // Empty commit means no commit_text is written.
    let committed = commit(&v);
    eprintln!("asr command commit (no buffer): '{committed}'");
    // With no buffer attached, __ASR_BUFFER__ expands to empty string,
    // so space commits nothing.
    assert!(
        committed.is_empty(),
        "expected empty commit, got '{committed}'"
    );
}

#[test]
fn asr_command_with_buffer_commits_voice_text() {
    // mock aura:Attach 的健康探针能打成功,`is_connected` 保持 true(确定不竞态)。
    let base = mock_aura();
    let mut eng = eng_with_aura(&base);
    // 直接写 shared voice state(真实 SSE 由 voice server 折叠;这里是测试 mock)。
    eng.voice_state().set_conn(ime_core::family::magic::voice_state::VoiceConn::Connected);
    eng.voice_state().seed_final("今天天气不错");

    // Type #asr
    for c in "#asr".chars() {
        eng.predict(KeyEvent::char(c));
    }
    // Space commits the voice text.
    let v = eng.predict(KeyEvent::space());
    assert_eq!(commit(&v), "今天天气不错");
}

/// 回归:`#asr` 在**非零 ctx** 会话中,voice 状态推进后 `magic_tick_ctx(真实 ctx)`
/// 必须返回带最新语音文本的视图。
///
/// 曾因 voice listener 发 `refresh_ui(StateView { ctx: 0 })` 而 fcitx5 的 C++
/// `onRefresh` 把 0 当作输入上下文指针 → `swift_ime_magic_tick(nullptr)` →
/// Rust 侧 `magic_tick_ctx(0)` 操作的是全新 ctx-0 状态机(非 Snippet)→ 返回
/// None → `apply_view` 从不执行,`#asr` 候选框永远停在"语音识别中..."。
/// 修复:refresh 用 [`ime_core::frontend::BROADCAST_CTX`] 让 C++ 广播到所有
/// 活动上下文;这里验证真实 ctx 的 magic_tick 能重建出语音文本候选。
#[test]
fn asr_magic_tick_refreshes_real_ctx_after_voice_advance() {
    // mock aura:Attach 探针成功,`is_connected` 保持 true。
    let base = mock_aura();
    let eng = eng_with_aura(&base);
    let ctx: usize = 0xCAFE; // fcitx 下 = 真实 InputContext 指针,非 0
    eng.voice_state().set_conn(ime_core::family::magic::voice_state::VoiceConn::Connected);

    // 在真实 ctx 输入 #asr → Snippet 态,VoiceMember 激活,候选是占位提示。
    let mut before = ImeView::empty();
    for c in "#asr".chars() {
        before = eng.predict_ctx(ctx, c);
    }
    let before_rows: Vec<String> = (0..before.candidate_count as usize)
        .map(|i| ImeView::str_field(&before.candidates[i].text).to_string())
        .collect();
    assert!(
        before_rows.iter().any(|c| c.contains("🎙")),
        "初始应是 🎙 占位提示: {before_rows:?}"
    );

    // voice 推进:模拟 listener 收到 StreamFragment → 折叠进 shared state。
    eng.voice_state().set_live_raw("你好");

    // 前端收到 BROADCAST_CTX 推送后,对真实 ctx 拉 magic_tick —— 必须重建
    // 出语音文本候选,而不是对 ctx 0 的(不存在的)#asr 会话返回 None。
    let after = eng
        .magic_tick_ctx(ctx)
        .expect("真实 ctx 的 #asr 会话应返回新视图");
    let top = ImeView::str_field(&after.candidates[0].text);
    assert!(top.contains("你好"), "候选应更新为语音文本,got top={top:?}");
    // 对不存在的 ctx(0,未输入过任何命令)应返回 None —— 广播的其余上下文天然跳过。
    assert!(
        eng.magic_tick_ctx(0).is_none(),
        "ctx 0 无 #asr 会话,应返回 None"
    );
}

/// 重连后**全量同步历史**:mock aura 在 `/api/results` 返回断连期间已定稿的句子,
/// 引擎重连(Attach 且未连)时应先清空旧历史、再拉 results 灌入 voice_state ——
/// 这样 `#asr` 重新打开时首个候选是断连期间说的那句话,而非旧残留。
#[test]
fn voice_reconnect_syncs_aura_history() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Duration;

    // mock aura:/health 200;`/api/asr_stream` 保持连接;`/api/results` 返回
    // 两条历史定稿(最旧 → 最新)。其它请求 200 close。
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { break };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut req = [0u8; 4096];
            let _ = s.read(&mut req);
            let line = String::from_utf8_lossy(&req);
            let body = if line.contains("/api/results") {
                r#"{"results":[{"window_id":1,"calibrated":"断连前的第一句"},{"window_id":2,"calibrated":"断连期间说的第二句"}]}"#.to_string()
            } else if line.contains("asr_") {
                // SSE 长连接:不回 body,保持连接。
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
                let _ = s.flush();
                std::thread::sleep(Duration::from_secs(30));
                continue;
            } else {
                r#"{"ok":true}"#.to_string() // /health 等
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });

    let mut eng = eng_with_aura(&format!("http://127.0.0.1:{port}"));
    // 先造一条"旧残留"(断连前的状态),验证重连会被清掉。
    eng.voice_state().seed_final("旧残留");

    // 触发 #asr → Attach → 未连 → 重连:reset + 拉 results 同步历史。
    for c in "#asr".chars() {
        eng.predict(KeyEvent::char(c));
    }
    // 给 io 线程时间处理 Attach / health / results。
    std::thread::sleep(Duration::from_millis(300));

    let (finals, _) = eng.voice_state().voice_candidates();
    eprintln!("reconnect finals: {finals:?}");
    assert!(
        finals.iter().any(|f| f.contains("断连期间说的第二句")),
        "重连后应同步到断连期间的新句, got {finals:?}"
    );
    assert!(
        !finals.iter().any(|f| f.contains("旧残留")),
        "旧残留应在重连时被清空, got {finals:?}"
    );
    let _ = Arc::new(());
}

#[test]
fn asr_prefix_shows_command_name() {
    let mut eng = ImeEngine::new();
    // Type partial #as → should show #asr as candidate.
    for c in "#as".chars() {
        eng.predict(KeyEvent::char(c));
    }
    let cands = eng.candidates();
    eprintln!("#as prefix candidates: {:?}", cands);
    // The matcher will show "#as" as preedit since #asr is in the trie.
    // The buffer shows the accumulated prefix.
    assert!(!eng.buffer().is_empty(), "should have buffer for '#as'");
    assert!(
        eng.buffer().starts_with("#as"),
        "buffer should start with '#as', got {:?}",
        eng.buffer()
    );
}

#[test]
fn recency_boost_promotes_recent_word() {
    let mut eng = ImeEngine::new();
    // Type and commit "大陆" → enters recency.
    for c in "dalu".chars() {
        eng.predict(KeyEvent::char(c));
    }
    eng.predict(KeyEvent::space()); // commit 大 (first candidate)
                                    // Now type "lu" — 陆 should get recency boost since we just saw "大".
    for c in "lu".chars() {
        eng.predict(KeyEvent::char(c));
    }
    let cands = eng.candidates();
    let lu_pos = cands.iter().position(|c| c == "陆");
    eprintln!(
        "After recency push(大): 陆 position = {lu_pos:?}, top-5: {:?}",
        cands.iter().take(5).collect::<Vec<_>>()
    );
    assert!(
        lu_pos.is_some(),
        "陆 should be in candidates after recency push"
    );
}

#[test]
fn phrase_initials_recall() {
    let mut eng = ImeEngine::new();

    // Compose "李正明" from "lizhengming".
    for c in "lizhengming".chars() {
        eng.predict(KeyEvent::char(c));
    }
    let li = eng.candidates().iter().position(|c| *c == "李").unwrap();
    eng.select_candidate(li);
    let zheng = eng.candidates().iter().position(|c| *c == "正").unwrap();
    eng.select_candidate(zheng);
    let ming = eng.candidates().iter().position(|c| *c == "明").unwrap();
    let v = eng.select_candidate(ming);
    assert_eq!(commit(&v), "李正明");

    // Now type initials "lzm" — should recall 李正明.
    for c in "lzm".chars() {
        eng.predict(KeyEvent::char(c));
    }
    let cands = eng.candidates();
    eprintln!("lzm recall: {:?}", cands.iter().take(5).collect::<Vec<_>>());
    assert!(
        cands.contains(&"李正明".to_string()),
        "lzm should recall 李正明, got {:?}",
        cands.iter().take(5).collect::<Vec<_>>()
    );
}
