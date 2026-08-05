#!/usr/bin/env bash
# fetch_emoji.sh — 从 Unicode CLDR annotations 生成 emoji 关键词词表。
#
# 输出: assets/dict/emoji.tsv — 每行 `keyword<TAB>emoji`:
#   - en 主注释关键词(竖线分隔拆分)+ en tts 短名
#   - zh 主注释关键词 + zh tts 短名
# 同一关键词去重;排序保证确定性(同关键词条目相邻,便于审查)。
#
# 数据源与 fcitx5 的 emoji 模块相同(CLDR annotations),覆盖约 1300 个常用
# emoji 的英文 + 中文关键词。加载: 引擎启动时 DICT::emoji.tsv
# (dev: apps/swift-ime/assets/dict/emoji.tsv, prod: /usr/share/swift-ime/dict)。
#
# 用法: cd apps/swift-ime && ./scripts/fetch_emoji.sh
set -euo pipefail

cd "$(dirname "$0")/.."  # apps/swift-ime/
OUT="assets/dict/emoji.tsv"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "── fetching CLDR annotations (en + zh) …"
curl -sfL --max-time 30 -o "$TMP/en.xml" \
    https://raw.githubusercontent.com/unicode-org/cldr/main/common/annotations/en.xml
curl -sfL --max-time 30 -o "$TMP/zh.xml" \
    https://raw.githubusercontent.com/unicode-org/cldr/main/common/annotations/zh.xml

python3 - "$TMP" "$OUT" <<'PY'
import sys
import xml.etree.ElementTree as ET

tmp, out = sys.argv[1], sys.argv[2]
rows = set()  # (keyword, emoji) 去重
for lang in ("en", "zh"):
    tree = ET.parse(f"{tmp}/{lang}.xml")
    for ann in tree.getroot().iter("annotation"):
        cp = ann.get("cp")
        if not cp:
            continue
        text = (ann.text or "").strip()
        if not text:
            continue
        if ann.get("type") == "tts":
            # tts 短名整体作为一个关键词("grinning face")——前缀匹配 "grinning" 即可命中
            rows.add((text, cp))
        else:
            # 主注释按 | 拆分("cheerful | cheery | face | …")
            for kw in text.split("|"):
                kw = kw.strip()
                if kw:
                    rows.add((kw, cp))

with open(out, "w", encoding="utf-8") as f:
    f.write("# emoji keyword table — generated from Unicode CLDR annotations (en+zh)\n")
    f.write("# format: keyword<TAB>emoji; regenerate with scripts/fetch_emoji.sh\n")
    for kw, cp in sorted(rows):
        f.write(f"{kw}\t{cp}\n")
print(f"generated {len(rows)} keyword rows → {out}")
PY

echo "done."
