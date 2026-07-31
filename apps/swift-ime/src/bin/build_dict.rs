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

use std::collections::HashMap;
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

    // Pass 1: count word frequencies.
    let mut freq: HashMap<String, u32> = HashMap::new();
    let mut entries: Vec<(String, String)> = Vec::new();

    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some((pinyin, word)) = line.split_once('\t') {
            let key = pinyin.replace(' ', "");
            if key.is_empty() || word.is_empty() { continue; }
            *freq.entry(word.to_string()).or_default() += 1;
            entries.push((key, word.to_string()));
        }
    }

    eprintln!("Building FST from {} entries...", entries.len());

    // Build FST via DictBuilder: code=pinyin, item=word, value=frequency.
    let mut builder = inputx_fsa::DictBuilder::new();
    for (pinyin, word) in &entries {
        let score = *freq.get(word).unwrap_or(&1) as u64;
        builder.insert(pinyin.as_bytes(), word.as_bytes(), score);
    }

    let fst_bytes = builder.finish();
    let mut f = std::fs::File::create(output).expect("create output");
    f.write_all(&fst_bytes).expect("write output");

    eprintln!("Done: {} → {} ({} bytes, {} entries)",
        input, output, fst_bytes.len(), entries.len());
}
