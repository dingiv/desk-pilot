#!/usr/bin/env python3
"""clean_emoji_llm.py — Clean the emoji dictionary with a local LLM, in batches.

The CLDR-derived keyword table (emoji/emoji.tsv, `kw<TAB>emoji`) is noisy:
ambiguous associations, rare words, no popularity signal. This script sends
batches of emojis (with their current candidate keywords) to a local
OpenAI-compatible model and asks it to, per emoji, emit the FINAL line
format directly:

    emoji<TAB>freq<TAB>kw1[<TAB>kw2[<TAB>kw3]]

  - freq: chat/social-scene popularity, 1..10000 (replaces uniform weights);
  - kws:  1..3 curated trigger spellings (lowercase English words, toneless
    pinyin, short hanzi words), ordered by "what the user would type".

Batches are written to <out>/out/batch_NNN.tsv and SKIPPED if present, so
the run is resumable; finally all batches merge into
<out>/emoji_cleaned.tsv (same line format).

Usage:
  python3 scripts/clean_emoji_llm.py \
      --emoji-table assets/dict/emoji/emoji.tsv \
      --out-dir tmp/emoji_clean \
      [--batch 100] [--limit 0] [--force] \
      [--api http://192.168.1.6:8080/v1] [--model Qwen3.8-27B-AWQ-INT4]
"""
import argparse
import json
import time
import urllib.error
import urllib.request
from pathlib import Path

PROJ = Path(__file__).resolve().parent.parent          # apps/swift-ime/

SYSTEM_PROMPT = """你是中文输入法的 emoji 词库清洗专家。输入是一批 emoji 及其现有候选触发词(来自 CLDR 自动标注,含噪声)。

对每个 emoji 恰好输出一行(直接就是目标格式,不要任何解释、表头或代码围栏):

emoji<TAB>freq<TAB>kw1[<TAB>kw2[<TAB>kw3]]

- freq:该 emoji 在中文聊天/社交场景的流行度,1~10000 整数。参考档位:🔥😂❤️👍 类顶流 8000-10000;常用表情 3000-8000;一般 800-3000;生僻/旗帜/古老符号 <300。
- kw:触发拼写,**最多 3 个**,按"用户最可能打出"排序。只允许小写英文单词、无声调全拼拼音、不超过 4 字的常用汉字词;剔除歧义联想和生僻词;每个长度 ≥2。
- 必须覆盖输入里的每一个 emoji,一个不能少,一行不多一行不少。"""


def parse_args():
    ap = argparse.ArgumentParser()
    ap.add_argument("--emoji-table", default=str(PROJ / "assets/dict/emoji/emoji.tsv"))
    ap.add_argument("--out-dir", default=str(PROJ / "tmp/emoji_clean"))
    ap.add_argument("--batch", type=int, default=100)
    ap.add_argument("--limit", type=int, default=0, help="clean only first N emojis (0 = all)")
    ap.add_argument("--force", action="store_true", help="re-run batches whose output exists")
    ap.add_argument("--only", type=int, default=-1,
                    help="run only batch N (diagnosis/rescue of a stuck batch)")
    ap.add_argument("--api", default="http://192.168.1.6:8080/v1")
    ap.add_argument("--model", default="Qwen3.8-27B-AWQ-INT4")
    ap.add_argument("--retries", type=int, default=3)
    return ap.parse_args()


def load_aggregate(path: str) -> list[dict]:
    """kw<TAB>emoji 平表 → [{emoji, cands:[kw...]}],按 CLDR 原序稳定。"""
    order: list[str] = []
    agg: dict[str, list[str]] = {}
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 2:
            continue
        kw, emoji = parts[0].strip().lower(), parts[1].strip()
        if len(kw) < 2 or not emoji:
            continue
        if emoji not in agg:
            agg[emoji] = []
            order.append(emoji)
        if kw not in agg[emoji]:
            agg[emoji].append(kw)
    return [{"emoji": e, "cands": agg[e]} for e in order]


def parse_tsv_rows(text: str, valid: set[str] | None) -> list[tuple[str, int, list[str]]]:
    """Tolerant parse of model output: `emoji<TAB>freq<TAB>kw1[<TAB>kw2[<TAB>kw3]]`."""
    rows: list[tuple[str, int, list[str]]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith("```"):
            continue
        parts = [p.strip() for p in line.split("\t")]
        if len(parts) < 3:
            continue
        emoji = parts[0]
        if emoji not in emoji and valid is not None and emoji not in valid:
            continue
        if valid is not None and emoji not in valid:
            continue
        try:
            freq = max(1, min(10000, int(parts[1])))
        except ValueError:
            continue
        kws: list[str] = []
        for kw in parts[2:5]:                     # 最多 3 个
            kw = kw.lower()
            if len(kw) >= 2 and kw not in kws:
                kws.append(kw)
        if kws:
            rows.append((emoji, freq, kws))
    return rows


def call_llm(api: str, model: str, payload_json: str, retries: int) -> str:
    body = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": payload_json},
        ],
        "temperature": 0.2,
        "max_tokens": 32000,
        # Qwen3 系推理模型:思考吃光输出预算(content=None/截断)。
        # 关闭思考 —— 清洗任务不需要,输出即目标 TSV 行。
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
            with urllib.request.urlopen(req, timeout=1800) as resp:
                data = json.load(resp)
            msg = data["choices"][0]["message"]
            # 推理模型:预算耗尽时 content 可能为 None,思考文本在
            # reasoning 里 —— 行解析器对两种文本都容错。
            return msg.get("content") or msg.get("reasoning") or ""
        except (urllib.error.URLError, KeyError, json.JSONDecodeError) as e:
            last_err = e
            wait = 5 * (attempt + 1)
            print(f"    attempt {attempt + 1} failed ({e}); retry in {wait}s", flush=True)
            time.sleep(wait)
    raise RuntimeError(f"LLM call failed after {retries} attempts: {last_err}")


def main() -> None:
    args = parse_args()
    out_dir = Path(args.out_dir)
    out_batch = out_dir / "out"
    out_batch.mkdir(parents=True, exist_ok=True)

    items = load_aggregate(args.emoji_table)
    if args.limit:
        items = items[: args.limit]
    total = sum(len(it["cands"]) for it in items)
    print(f"aggregate: {len(items)} emojis, {total} keywords "
          f"({args.emoji_table})")

    batches = [items[i:i + args.batch] for i in range(0, len(items), args.batch)]
    for bi, batch in enumerate(batches):
        if args.only >= 0 and bi != args.only:
            continue
        dst = out_batch / f"batch_{bi:03d}.tsv"
        valid = {it["emoji"] for it in batch}
        if dst.exists() and not args.force:
            print(f"  batch {bi:03d}: exists, skip")
            continue
        payload = json.dumps(
            [{"emoji": it["emoji"], "cands": it["cands"]} for it in batch],
            ensure_ascii=False,
        )
        t0 = time.time()
        text = call_llm(args.api, args.model, payload, args.retries)
        rows = parse_tsv_rows(text, valid)
        with dst.open("w", encoding="utf-8") as f:
            for emoji, freq, kws in rows:
                f.write("\t".join([emoji, str(freq), *kws]) + "\n")
        missing = len(valid) - len({r[0] for r in rows})
        print(f"  batch {bi:03d}: {len(rows)}/{len(batch)} emojis "
              f"({missing} missing) in {time.time() - t0:.0f}s -> {dst}",
              flush=True)

    # Merge all batches (freq 降序).
    merged: dict[str, tuple[int, list[str]]] = {}
    for dst in sorted(out_batch.glob("batch_*.tsv")):
        for emoji, freq, kws in parse_tsv_rows(
                dst.read_text(encoding="utf-8"), None):
            merged[emoji] = (freq, kws)
    final = out_dir / "emoji_cleaned.tsv"
    with final.open("w", encoding="utf-8") as f:
        f.write("# @type: emoji-freq\n")
        f.write(f"# cleaned by {args.model}; format: emoji<TAB>freq<TAB>kw... (max 3 kws)\n")
        for emoji in sorted(merged, key=lambda e: -merged[e][0]):
            freq, kws = merged[emoji]
            f.write("\t".join([emoji, str(freq), *kws]) + "\n")
    print(f"merged {len(merged)} emojis -> {final}")


if __name__ == "__main__":
    main()
