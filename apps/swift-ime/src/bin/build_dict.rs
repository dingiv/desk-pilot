//! Build tool: convert rime-ice TSV → inputx FST binary.
//!
//! FST format: key = pinyin\x00word, value = frequency score.
//! The frequency is derived from occurrence counts in the TSV.
//!
//! Usage:
//!   cargo run --bin build_dict -- assets/dict/rime-ice.tsv assets/dict/rime-ice.fst
//!
//! The resulting .fst file is loaded by PinyinFamily at startup for
//! unified L1 frequency-based scoring — same model as fcitx5's sc.dict.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).expect("usage: build_dict <input.tsv> [output.fst]");
    let output = args.get(2).map(|s| s.as_str()).unwrap_or("rime-ice.fst");

    // Cache check: skip rebuild if TSV hasn't changed since FST was built.
    if let (Ok(tsv_meta), Ok(fst_meta)) = (
        std::fs::metadata(input),
        std::fs::metadata(output),
    ) {
        if let (Ok(tsv_mtime), Ok(fst_mtime)) = (tsv_meta.modified(), fst_meta.modified()) {
            if tsv_mtime <= fst_mtime {
                eprintln!("FST cache hit: {output} is newer than {input}, skipping rebuild.");
                return;
            }
        }
    }

    eprintln!("Loading {input}...");
    let data = std::fs::read_to_string(input).expect("read input");

    // Parse TSV. If 3rd column exists, use it as weight.
    // Otherwise use occurrence count as proxy weight.
    let has_weights = data.lines().next()
        .map(|l| l.split('\t').count() >= 3).unwrap_or(false);

    let mut entries: Vec<(String, String, u64)> = Vec::new();
    let mut occ: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 { continue; }
        let pinyin = parts[0].replace(' ', "");
        let word = parts[1].to_string();
        if has_weights {
            let weight: u64 = parts.get(2).and_then(|w| w.parse().ok()).unwrap_or(100);
            if !pinyin.is_empty() && !word.is_empty() {
                entries.push((pinyin, word, weight));
            }
        } else {
            *occ.entry(word.clone()).or_default() += 1;
            entries.push((pinyin, word, 0)); // weight filled in pass 2
        }
    }

    // Pass 2 for occurrence-based weights: use occurrence count × 100.
    if !has_weights {
        for (_, word, weight) in &mut entries {
            *weight = *occ.get(word).unwrap_or(&1) as u64 * 100;
        }
    }

    eprintln!("Building FST from {} entries...", entries.len());

    // Build FST via DictBuilder: code=pinyin, item=word, value=weight.
    let mut builder = inputx_fsa::DictBuilder::new();
    for (pinyin, word, weight) in &entries {
        builder.insert(pinyin.as_bytes(), word.as_bytes(), *weight);
    }

    let fst_bytes = builder.finish();
    let mut f = std::fs::File::create(output).expect("create output");
    f.write_all(&fst_bytes).expect("write output");

    eprintln!("Done: {} → {} ({} bytes, {} entries)",
        input, output, fst_bytes.len(), entries.len());
}
