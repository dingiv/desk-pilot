#!/usr/bin/env python3
"""build_en_dict.py — Convert hermitdave/FrequencyWords en_full into the
embedded English dictionary (word + real corpus frequency, single source).

Raw source (NOT in git, ~20MB):
    https://raw.githubusercontent.com/hermitdave/FrequencyWords/master/content/2016/en/en_full.txt
    Format: `word count` (space-separated, raw subtitle occurrence counts).

Fetch it manually into apps/swift-ime/tmp/en_full.txt, then:

    python3 scripts/build_en_dict.py [raw.txt] [out.tsv]

Output (assets/dict/hermitdave/en_freq.tsv): `word\\tcount` TSV, one entry
per word. Engine side parses it as DictType::Frequency and decile-normalizes.

Filtering (replaces the former SCOWL wordlist entirely):
  - letters only (`[A-Za-z]+`), length 2..=20 — nothing the user cannot type;
  - count >= 150 for long words — below that the subtitle corpus is mostly
    OCR noise, mangled proper nouns and ultra-rare jargon (pisani/cutaneous/
    yingying class) that would outrank Chinese candidates via exact scoring;
  - count >= 3000 for short words (<=4 letters, incl. cd/ok/us) — the
    short-word band is where subtitle artifacts concentrate;
  - case preserved (proper nouns keep their casing; matching is
    case-insensitive in the engine).
"""
import re
import sys
from pathlib import Path

PROJ = Path(__file__).resolve().parent.parent          # apps/swift-ime/
RAW_DEFAULT = PROJ / "tmp" / "en_full.txt"
OUT_DEFAULT = PROJ / "assets" / "dict" / "hermitdave" / "en_freq.tsv"

MIN_COUNT = 150
# 短词(≤4 字母)字幕语料里伪影密度极高(clea/th/im/js 类截断与撇号丢失),
# 需要真实高频(ok/cd/us)才能入围。
MIN_COUNT_SHORT = 3000
SHORT_LEN = 4
# 缩略形去撇号变体(I'm→im 类 4 字母内的被短词门槛滤掉;这里兜住更长的)。
# 注意保留真词:wont(习惯)/ lets(let 的三单)不在列。
CONTRACTION_ARTIFACTS = {
    "im", "th",  # I'm 去撇号 / 截断伪影(高频短词,门槛滤不掉)
    "didnt", "doesnt", "isnt", "wasnt", "arent", "werent", "havent",
    "hasnt", "hadnt", "couldnt", "shouldnt", "wouldnt", "cant", "dont",
    "thats", "whats", "youre", "theyre", "youve", "theyve", "youll",
    "theyll", "whos", "hows", "heres", "theres", "wheres",
}
WORD_RE = re.compile(r"^[A-Za-z]{2,20}$")


def main() -> None:
    raw = Path(sys.argv[1]) if len(sys.argv) > 1 else RAW_DEFAULT
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else OUT_DEFAULT
    if not raw.exists():
        sys.exit(f"raw wordlist not found: {raw}\n(download en_full.txt, see docstring)")

    kept = 0
    lines_out: list[str] = [
        "# @type: frequency",
        "# Source: hermitdave/FrequencyWords en_full (raw subtitle counts, count>=30, [A-Za-z]{2,20})",
        "# Regenerate: python3 scripts/build_en_dict.py",
    ]
    with raw.open(encoding="utf-8", errors="replace") as f:
        for line in f:
            parts = line.split()
            if len(parts) != 2:
                continue
            word, count = parts
            n = int(count)
            if not WORD_RE.match(word) or word in CONTRACTION_ARTIFACTS:
                continue
            threshold = MIN_COUNT_SHORT if len(word) <= SHORT_LEN else MIN_COUNT
            if n < threshold:
                continue
            lines_out.append(f"{word}\t{n}")
            kept += 1

    out.write_text("\n".join(lines_out) + "\n", encoding="utf-8")
    print(f"en_freq.tsv: kept {kept} words -> {out}")


if __name__ == "__main__":
    main()
