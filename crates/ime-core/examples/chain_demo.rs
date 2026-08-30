//! 链式预测演示(整引擎链路):`cargo run -p ime-core --example chain_demo`
use ime_core::engine::ImeEngine;
use ime_core::frontend::ImeView;
use ime_core::router::KeyEvent;

const RIME_ICE: &str = "/workspaces/gui_agent/desk-pilot/apps/swift-ime/assets/dict/rime-ice.fst";

fn type_str(e: &mut ImeEngine, s: &str) {
    for c in s.chars() {
        e.key(KeyEvent::char(c));
    }
}

fn show(e: &mut ImeEngine, q: &str) {
    type_str(e, q);
    let cands = e.candidates();
    println!("{q:>26} → {}", cands.iter().take(4).cloned().collect::<Vec<_>>().join(" / "));
    e.key(KeyEvent::escape());
}

fn main() {
    let mut e = ImeEngine::new();
    let _ = e.load_dict(RIME_ICE);

    // P0 文本链
    show(&mut e, "ti'an");

    // P1:不感知命令(#date 静态)→ 预测与上游首选拼接
    show(&mut e, "nihao'#date");

    // P1:级联(两个 #date 左折叠)
    show(&mut e, "nihao'#date'#date");

    // P1:命令段未知(#zzz)→ 上游预测可直接选中
    show(&mut e, "mingtian'#zzz");

    // P1:回退 —— 命令链退格回上游,仍是拼音组合
    type_str(&mut e, "nihao'#date");
    for _ in 0..6 {
        e.key(KeyEvent::backspace()); // 删到 "niha"
    }
    println!(
        "{:>26} → {}   (退格回上游:仍是拼音组合)",
        "nihao'#date + 6×BS",
        e.candidates().iter().take(3).cloned().collect::<Vec<_>>().join(" / ")
    );
    e.key(KeyEvent::escape());

    // P1:提交 —— 选中拼接候选 nihao'#date 的首条
    type_str(&mut e, "nihao'#date");
    let v = e.key(KeyEvent::char('1'));
    println!(
        "{:>26} → commit {:?}",
        "nihao'#date + 1",
        ImeView::str_field(&v.commit_text)
    );

    // P2:空链整页上下文(shijian''#concat:整页候选拼接)
    show(&mut e, "shijian''#concat");

    // 对照:单链 First(shijian'#concat 只拼首选)
    show(&mut e, "shijian'#concat");

    // P2:#concat 无上游 → 用法提示(interactive,不上屏)
    show(&mut e, "#concat");

    // 对照:单独 # 无 ' 前导 → 旧终结符行为
    type_str(&mut e, "nihao#date");
    let v = e.key(KeyEvent::space());
    println!(
        "{:>26} → commit {:?}   (无 ':旧行为)",
        "nihao#date + Space",
        ImeView::str_field(&v.commit_text)
    );
}
