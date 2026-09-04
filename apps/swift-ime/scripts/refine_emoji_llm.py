#!/usr/bin/env python3
"""refine_emoji_llm.py — Round-2 calibration of the cleaned emoji dictionary.

Round 1 (clean_emoji_llm.py) scores every emoji in isolation, so absolute
scores drift between batches. This round fixes that with CROSS-BATCH
comparison:

  1. load the merged round-1 file (already sorted by freq desc);
  2. split it into `--segments` (100) equal bands of ~20 emojis each;
  3. build `--batches` (20) work batches by taking ONE emoji from EVERY band
     — each batch spans the whole frequency range, so the model can compare
     across tiers inside one context;
  4. ask the model to CALIBRATE: only adjust the freq numbers that are clearly
     off within the batch, keep reasonable ones. No new content.

The model answers two columns only (`emoji<TAB>freq`); kws are filled back
from round 1 by this script — mechanically guaranteeing "adjust only, no
generation". Output: <out>/round2/batch_NNN.tsv (resumable), merged into
<out>/emoji_cleaned_r2.tsv (freq desc).

Usage:
  python3 scripts/refine_emoji_llm.py \
      [--in tmp/emoji_clean/emoji_cleaned.tsv] \
      [--out-dir tmp/emoji_clean/round2] \
      [--segments 100] [--batches 20] [--batch-size 100] \
      [--api http://192.168.1.6:8080/v1] [--model Qwen3.8-27B-AWQ-INT4]
      [--force] [--only N]
"""
import argparse
import json
import math
import time
import urllib.error
import urllib.request
from pathlib import Path

PROJ = Path(__file__).resolve().parent.parent          # apps/swift-ime/

SYSTEM_PROMPT = """你是 emoji 流行度校准专家。输入一批 emoji 及其当前流行度分(freq,1~10000 整数),这批 emoji 覆盖了从顶流到生僻的完整区间。

请在批次内横向比较:freq 明显不合理的(与同类 emoji 相比过高/过低)做**微调**;基本合理的保持原分。参考锚点:🔥😂❤️👍 类日常顶流 8000-10000;常用表情 3000-8000;偶尔出现 800-3000;生僻/旗帜/数学符号 <300。

只输出两列 TSV(每行,不要解释/表头/围栏):

emoji<TAB>freq

- kws 一概不用输出,不要生成任何新内容;
- 必须覆盖输入里的每一个 emoji,一个不能少;
- 大部分行应保持原分,只调整确实不对的。"""


def parse_args():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="infile",
                    default=str(PROJ / "tmp/emoji_clean/emoji_cleaned.tsv"))
    ap.add_argument("--out-dir", default=str(PROJ / "tmp/emoji_clean/round2"))
    ap.add_argument("--segments", type=int, default=100)
    ap.add_argument("--batches", type=int, default=20)
    ap.add_argument("--force", action="store_true")
    ap.add_argument("--only", type=int, default=-1,
                    help="run only batch N (diagnosis/rescue)")
    ap.add_argument("--api", default="http://192.168.1.6:8080/v1")
    ap.add_argument("--model", default="Qwen3.8-27B-AWQ-INT4")
    ap.add_argument("--retries", type=int, default=3)
    return ap.parse_args()


def load_round1(path: str) -> list[tuple[str, int, list[str]]]:
    """emoji<TAB>freq<TAB>kw... → [(emoji, freq, kws)],保持文件降序。"""
    rows = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) < 3:
            continue
        try:
            freq = max(1, min(10000, int(parts[1])))
        except ValueError:
            continue
        rows.append((parts[0], freq, parts[2:]))
    return rows


def parse_calib(text: str, valid: set[str]) -> dict[str, int]:
    """Two-column TSV → {emoji: freq}; skips junk, clamps 1..10000."""
    out: dict[str, int] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith("```"):
            continue
        parts = line.split("\t")
        if len(parts) < 2:
            continue
        emoji = parts[0].strip()
        if emoji not in valid:
            continue
        try:
            out[emoji] = max(1, min(10000, int(parts[1])))
        except ValueError:
            continue
    return out


def call_llm(api: str, model: str, payload_json: str, retries: int) -> str:
    body = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": payload_json},
        ],
        "temperature": 0.2,
        "max_tokens": 32000,
        # Qwen3 思考会吃光输出预算,清洗/校准任务直接关闭。
        "chat_template_kwargs": {"enable_thinking": False},
    }).encode()
    last_err = None
    for attempt in range(retries):
        try:
            req = urllib.request.Request(
                f"{api.rstrip('/')}/chat/completions",
                data=body,
                headers={"Content-Type": "application/json",
                         "Authorization": "Bearer local-no-key"},
            )
            with urllib.request.urlopen(req, timeout=240) as resp:
                data = json.load(resp)
            msg = data["choices"][0]["message"]
            return msg.get("content") or msg.get("reasoning") or ""
        except (urllib.error.URLError, KeyError, json.JSONDecodeError) as e:
            last_err = e
            wait = 5 * (attempt + 1)
            print(f"    attempt {attempt + 1} failed ({e}); retry in {wait}s",
                  flush=True)
            time.sleep(wait)
    raise RuntimeError(f"LLM call failed after {retries} attempts: {last_err}")


def main() -> None:
    args = parse_args()
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    rows = load_round1(args.infile)
    freq_of = {e: f for e, f, _ in rows}
    print(f"round1: {len(rows)} emojis from {args.infile}")

    # 1. Equal bands over the desc-sorted file (~20 emojis per band).
    seg_size = math.ceil(len(rows) / args.segments)
    bands = [rows[i:i + seg_size] for i in range(0, len(rows), seg_size)]
    print(f"bands: {len(bands)} × ~{seg_size}")

    # 2. Batch j takes the j-th emoji of every band → spans the full range.
    batches: list[list[tuple[str, int]]] = []
    for j in range(args.batches):
        batch = [(e, f) for band in bands if j < len(band)
                 for e, f, _ in [band[j]]]
        if batch:
            batches.append(batch)
    print(f"work batches: {len(batches)} × ~{len(batches[0])}")

    n_moved = 0
    for bi, batch in enumerate(batches):
        dst = out_dir / f"batch_{bi:03d}.tsv"
        valid = {e for e, _ in batch}
        if dst.exists() and not args.force:
            print(f"  batch {bi:03d}: exists, skip")
            continue
        payload = json.dumps([{"emoji": e, "freq": f} for e, f in batch],
                             ensure_ascii=False)
        t0 = time.time()
        text = call_llm(args.api, args.model, payload, args.retries)
        calib = parse_calib(text, valid)
        # 回填:只取校准后的 freq,kws 原样从 round1 带回(不生成新内容)。
        lines = []
        moved = 0
        for e, old_f in batch:
            new_f = calib.get(e, old_f)
            if new_f != old_f:
                moved += 1
            kws = next(k for ee, _, k in rows if ee == e)
            lines.append("\t".join([e, str(new_f), *kws]))
        dst.write_text("\n".join(lines) + "\n", encoding="utf-8")
        n_moved += moved
        print(f"  batch {bi:03d}: {len(calib)}/{len(batch)} covered, "
              f"{moved} adjusted in {time.time() - t0:.0f}s -> {dst}",
              flush=True)

    # Merge → sorted desc.
    merged: dict[str, tuple[int, list[str]]] = {}
    for dst in sorted(out_dir.glob("batch_*.tsv")):
        for line in dst.read_text(encoding="utf-8").splitlines():
            parts = line.split("\t")
            if len(parts) >= 3:
                merged[parts[0]] = (int(parts[1]), parts[2:])
    final = PROJ / "tmp/emoji_clean/emoji_cleaned_r2.tsv"
    with final.open("w", encoding="utf-8") as f:
        f.write("# @type: emoji-freq\n")
        f.write(f"# round-2 calibrated by {args.model}; freq desc\n")
        for emoji in sorted(merged, key=lambda e: -merged[e][0]):
            freq, kws = merged[emoji]
            f.write("\t".join([emoji, str(freq), *kws]) + "\n")
    print(f"merged {len(merged)} emojis -> {final}")


if __name__ == "__main__":
    main()
