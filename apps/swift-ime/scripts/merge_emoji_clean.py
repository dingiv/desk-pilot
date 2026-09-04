#!/usr/bin/env python3
"""merge_emoji_clean.py — Merge clean_emoji_llm batch outputs into one file.

Reads <out-dir>/out/batch_*.tsv lines (emoji<TAB>freq<TAB>kw1[<TAB>kw2[<TAB>kw3]]),
merges them (a later batch wins per emoji), sorts by freq DESCENDING and
writes <out-dir>/emoji_cleaned.tsv.

Usage:
  python3 scripts/merge_emoji_clean.py [--out-dir tmp/emoji_clean] [--output NAME]
"""
import argparse
from pathlib import Path

PROJ = Path(__file__).resolve().parent.parent          # apps/swift-ime/
HEADER = "# @type: emoji-freq"


def parse_rows(text: str):
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) < 3:
            continue
        emoji = parts[0].strip()
        try:
            freq = max(1, min(10000, int(parts[1])))
        except ValueError:
            continue
        kws: list[str] = []
        for kw in parts[2:]:
            kw = kw.strip().lower()
            if len(kw) >= 2 and kw not in kws and len(kws) < 3:
                kws.append(kw)
        if emoji and kws:
            yield emoji, freq, kws


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", default=str(PROJ / "tmp/emoji_clean"))
    ap.add_argument("--output", default="emoji_cleaned.tsv")
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    merged: dict[str, tuple[int, list[str]]] = {}
    files = sorted(out_dir.glob("out/batch_*.tsv"))
    if not files:
        raise SystemExit(f"no batch files under {out_dir}/out/")
    for f in files:
        for emoji, freq, kws in parse_rows(f.read_text(encoding="utf-8")):
            merged[emoji] = (freq, kws)

    rows = sorted(merged.items(), key=lambda kv: (-kv[1][0], kv[0]))
    dst = out_dir / args.output
    with dst.open("w", encoding="utf-8") as f:
        f.write(f"{HEADER}\n")
        f.write("# merged from out/batch_*.tsv, sorted by freq desc\n")
        for emoji, (freq, kws) in rows:
            f.write("\t".join([emoji, str(freq), *kws]) + "\n")

    freqs = [freq for _, (freq, _) in rows]
    print(f"{len(files)} batches -> {len(rows)} emojis -> {dst}")
    print(f"freq range: {freqs[0]} (top) .. {freqs[-1]} (tail); "
          f"sorted desc: {all(a >= b for a, b in zip(freqs, freqs[1:]))}")


if __name__ == "__main__":
    main()
