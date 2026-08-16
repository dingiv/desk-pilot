//! 全局排序黄金测试 —— 锁住跨家族候选层次(顺序,不锁分数)。
//!
//! 动机(issues-round5.md #1):跨家族顺序是 ~15 个打分常数的涌现结果,
//! 没有任何单一处声明。历史上每次排序 bug(jix/jixu 同分、emoji 压英文
//! 本尊、闹着压闹钟、机械的压继续)都是两个家族的常数在分数空间意外相撞。
//! 此表用**默认权重**(直接构造引擎,不走 swift-ime.yaml,不受用户调参
//! 影响)断言代表性输入的预期顺序 —— 任何常数调整打破层次立刻红灯。
//!
//! Run: cargo test -p swift-ime --test global_ranking

use ime_core::engine::{ImeEngine, InputEvent};
use std::path::Path;

fn dict(name: &str) -> Option<String> {
    let pkg = env!("CARGO_MANIFEST_DIR");
    let p = format!("{pkg}/assets/dict/{name}");
    Path::new(&p).exists().then(|| p)
}

/// 默认权重 + rime-ice + emoji 词表的引擎。
fn engine() -> ImeEngine {
    let e = ImeEngine::new();
    let fst = dict("rime-ice.fst").expect("rime-ice.fst missing (run fetch_dict.sh)");
    e.load_dict(&fst).expect("load rime-ice");
    if let Some(emoji) = dict("emoji.tsv") {
        e.load_emoji_dict(&emoji).expect("load emoji.tsv");
    }
    e
}

fn rank(e: &mut ImeEngine, input: &str) -> Vec<(String, String, String)> {
    for c in input.chars() {
        e.predict(InputEvent::char(c));
    }
    e.candidates_detailed()
        .into_iter()
        .map(|d| (d.text, d.family.to_string(), d.source.to_string()))
        .collect()
}

/// 断言 `above` 排在 `below` 前(同输入的候选列表内)。
fn assert_above(input: &str, ranked: &[(String, String, String)],
                above: &str, below: &str) {
    let pa = ranked.iter().position(|(t, _, _)| t == above)
        .unwrap_or_else(|| panic!("{input}: {above} 不在候选中: {ranked:?}"));
    let pb = ranked.iter().position(|(t, _, _)| t == below)
        .unwrap_or_else(|| panic!("{input}: {below} 不在候选中: {ranked:?}"));
    assert!(pa < pb, "{input}: 预期 {above} 排在 {below} 前,实际 {pa} vs {pb}\n{ranked:?}");
}

fn top1_is(input: &str, ranked: &[(String, String, String)], expected: &str) {
    assert_eq!(ranked.first().map(|(t, _, _)| t.as_str()), Some(expected),
        "{input}: 预期 #1 = {expected}\n{ranked:?}");
}

// ── 拼音家族内部:全拼区分度(线性重标,不贴顶)────────────────────────

#[test]
fn full_pinyin_keeps_frequency_discrimination() {
    let mut e = engine();
    let r = rank(&mut e, "jixu");
    // 501276(继续)> 164505(急须)—— clamp 时代的回归会全部同分贴顶。
    top1_is("jixu", &r, "继续");
    assert_above("jixu", &r, "继续", "急须");
    // 顶流封顶 0.90(经 short_word_bonus 0.91),不顶满 1.0。
    let mut e2 = engine();
    for c in "jixu".chars() { e2.predict(InputEvent::char(c)); }
    let detailed = e2.candidates_detailed();
    let ji_xu = detailed.iter().find(|d| d.text == "继续").unwrap();
    assert!(ji_xu.score < 0.95, "顶流应封顶留白: {}", ji_xu.score);
}

// ── 拼音前缀联想:高频远词 > 低频近词;SCAN_CAP 不饿死高频 ──────────────

#[test]
fn prefix_association_prefers_high_frequency() {
    let mut e = engine();
    let r = rank(&mut e, "naozh");
    // 闹钟(84165, 差3) > 闹着(9965, 差1):免费额度覆盖"半截声母到完整
    // 音节"的典型差(zh→zhong);SCAP_CAP 字典序饿死回归(jixu 排 jixi* 后)。
    top1_is("naozh", &r, "闹钟");
    assert_above("naozh", &r, "闹钟", "闹着");

    let mut e2 = engine();
    let r2 = rank(&mut e2, "jix");
    // jixu(继续, 501276)字典序排在几百个 jixi* 之后 —— 256 池会饿死它。
    top1_is("jix", &r2, "继续");
}

// ── 英文本尊 > 同名词的 emoji 关键词 ────────────────────────────────────

#[test]
fn english_word_outranks_its_emoji_namesake() {
    let mut e = engine();
    let r = rank(&mut e, "clea");
    // clean 是英文词本尊(0.441),🧼 的 CLDR 关键词恰好也是 clean ——
    // 本尊必须压过 emoji 前缀命中。
    top1_is("clea", &r, "clean");
    assert_above("clea", &r, "clean", "🫧");
}

#[test]
fn english_exact_above_its_prefixes() {
    let mut e = engine();
    let r = rank(&mut e, "swift");
    top1_is("swift", &r, "swift");
    assert_above("swift", &r, "swift", "swifts");
}

// ── emoji 层次:en exact > emoji exact > 简拼 > emoji 前缀 ──────────────

#[test]
fn emoji_exact_below_english_exact() {
    let mut e = engine();
    let r = rank(&mut e, "smile");
    // emoji.tsv 里 smile 关键词 → 🥲(外部表覆盖内置表 😊)。
    top1_is("smile", &r, "smile");
    assert_above("smile", &r, "smile", "🥲");
}

#[test]
fn two_letter_emoji_keyword_demoted_below_jianpin() {
    let mut e = engine();
    let r = rank(&mut e, "cd");
    // cd 是 📀 的关键词(≤2 字母,即使完整命中也降前缀档)—— 中文简拼
    // (承担 0.503)必须压过它(0.36)。
    top1_is("cd", &r, "承担");
    assert_above("cd", &r, "承担", "📀");
}

// ── 单音节中文 > 一切外族 ───────────────────────────────────────────────

#[test]
fn single_syllable_chinese_on_top() {
    let mut e = engine();
    let r = rank(&mut e, "de");
    top1_is("de", &r, "的");
}

// ── 学习边界:纯 ASCII 不进拼音单词本 ──────────────────────────────────

#[test]
fn pure_ascii_never_becomes_pinyin_phrase() {
    // name(纯英文)即使被提交也不能以 pinyin/phrase 身份出现 —— 英文自生词
    // 走 english/user 体系。回归:name 曾以 0.937 pinyin/phrase 霸榜。
    let mut e = engine();
    for c in "name".chars() { e.predict(InputEvent::char(c)); }
    let idx = e.candidates().iter().position(|c| c == "那么").expect("那么 present");
    e.select_candidate(idx); // 提交 那么(中文,正常学习路径)
    // 再提交一次英文 name(选中英文候选,若存在)。
    for c in "name".chars() { e.predict(InputEvent::char(c)); }
    if let Some(i) = e.candidates().iter().position(|c| c == "name") {
        e.select_candidate(i);
    }
    for c in "name".chars() { e.predict(InputEvent::char(c)); }
    let detailed = e.candidates_detailed();
    let name_cand = detailed.iter().find(|d| d.text == "name");
    if let Some(d) = name_cand {
        assert_ne!(d.family, "pinyin",
            "纯 ASCII 不得成为拼音候选: {d:?}\n{detailed:?}");
    }
    // 那么 仍然 #1。
    let r: Vec<(String, String, String)> = detailed.into_iter()
        .map(|d| (d.text, d.family.to_string(), d.source.to_string())).collect();
    top1_is("name", &r, "那么");
}

// ── 简拼与全拼一致性 ────────────────────────────────────────────────────

#[test]
fn jianpin_and_full_pinyin_agree_on_top() {
    // 同词的简拼与全拼路径必须给出同一个 #1(历史上 jix 曾给出不同结果)。
    let mut e1 = engine();
    let r1 = rank(&mut e1, "jix");
    let mut e2 = engine();
    let r2 = rank(&mut e2, "jixu");
    assert_eq!(r1.first().map(|(t, _, _)| t), r2.first().map(|(t, _, _)| t),
        "jix 与 jixu 的 #1 必须一致: {r1:?} vs {r2:?}");
}
