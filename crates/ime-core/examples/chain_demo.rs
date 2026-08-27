//! 链式预测演示(整引擎链路):`cargo run -p ime-core --example chain_demo`
use ime_core::engine::ImeEngine;
use ime_core::router::KeyEvent;

const RIME_ICE: &str = "/workspaces/gui_agent/desk-pilot/apps/swift-ime/assets/dict/rime-ice.fst";

fn main() {
    let mut e = ImeEngine::new();
    match e.load_dict(RIME_ICE) {
        Ok(n) => println!("[dict] rime-ice: {n} entries"),
        Err(err) => println!("[dict] load failed: {err}(组合无成词加成)"),
    }

    for q in ["tian", "ti'an", "xi'an", "ti"] {
        for c in q.chars() {
            e.key(KeyEvent::char(c));
        }
        let cands = e.candidates();
        println!("{q:>10} → {}", cands.iter().take(6).cloned().collect::<Vec<_>>().join(" / "));
        e.key(KeyEvent::escape());
    }

    // raw 回退:Enter 提交原始输入(含 ')
    for c in "ti'an".chars() {
        e.key(KeyEvent::char(c));
    }
    let v = e.key(KeyEvent::enter());
    println!(
        "{:>10} → commit {:?}",
        "ti'an⏎",
        ime_core::platform::ImeView::str_field(&v.commit_text)
    );
}
