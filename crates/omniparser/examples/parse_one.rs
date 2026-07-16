//! One-shot demo: parse a screenshot PNG → write the annotated Set-of-Mark PNG
//! and print the element table.
//!
//! Run from anywhere:
//!   ./crates/omniparser/fetch_weights.sh
//!   cargo run -p omniparser --example parse_one -- <in.png> [out_som.png]

use omniparser::{OmniParser, OmniParserConfig};

fn main() {
    let in_path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: parse_one <in.png> [out_som.png]");
            std::process::exit(1);
        }
    };
    let out_path = std::env::args().nth(2).unwrap_or_else(|| "som.png".to_string());

    // Default to the crate's bundled weights, overridable via env.
    let weights = std::env::var("OMNIPARSER_WEIGHTS").unwrap_or_else(|_| {
        format!("{}/weights/icon_detect.onnx", env!("CARGO_MANIFEST_DIR"))
    });

    let img = match image::open(&in_path) {
        Ok(d) => d.to_rgb8(),
        Err(e) => {
            eprintln!("[parse_one] open {in_path}: {e}");
            std::process::exit(2);
        }
    };
    eprintln!(
        "[parse_one] {}x{}  →  YOLO detect (weights: {})",
        img.width(),
        img.height(),
        weights
    );

    let mut parser = match OmniParser::new(OmniParserConfig::new(&weights)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[parse_one] init failed: {e}");
            std::process::exit(3);
        }
    };
    let res = match parser.parse(&img) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[parse_one] parse failed: {e}");
            std::process::exit(4);
        }
    };

    if let Err(e) = std::fs::write(&out_path, &res.annotated_png) {
        eprintln!("[parse_one] write {out_path}: {e}");
        std::process::exit(5);
    }
    eprintln!("[parse_one] {} elements  →  {out_path}", res.elements.len());

    for e in &res.elements {
        let b = e.bbox;
        println!(
            "  #{:<3} icon  [{:>4.0}, {:>4.0}, {:>4.0}, {:>4.0}]",
            e.mark, b.x1, b.y1, b.x2, b.y2
        );
    }
}
