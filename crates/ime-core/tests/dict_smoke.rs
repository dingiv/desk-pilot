#[test]
fn smoke_large_dict_has_common_words() {
    use ime_core::large_dict::LargeDict;
    let mut dict = LargeDict::new();
    let n = dict.load_from_tsv_file("apps/swift-ime/assets/dict/rime-ice.tsv")
        .unwrap_or_else(|_| {
            dict.load_from_tsv_file("../../apps/swift-ime/assets/dict/rime-ice.tsv")
                .unwrap_or(0)
        });
    assert!(n > 500_000, "expected 500k+ entries, got {n}");

    for (py, want) in &[
        ("shuru", "输入"), ("dakai", "打开"), ("nihao", "你好"),
        ("zhongguo", "中国"), ("women", "我们"), ("dajia", "大家"),
    ] {
        let r = dict.exact(py);
        assert!(r.iter().any(|w| w == want),
            "rime-ice missing: {py} → {want}, top-5: {:?}", &r[..r.len().min(5)]);
    }

    // Words we added to base.tsv that rime-ice might not have
    let base_only = &[("qianyi", "迁移"), ("baocun", "保存"), ("sousuo", "搜索")];
    for (py, want) in base_only {
        let r = dict.exact(py);
        println!("base.tsv word {py} → {want}: {}",
            if r.iter().any(|w| w == want) { "in rime-ice too" } else { "only in base.tsv" });
    }
}
