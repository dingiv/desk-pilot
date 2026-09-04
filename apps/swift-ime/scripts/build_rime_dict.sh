#!/usr/bin/env bash
# build_rime_dict.sh — Download and convert RIME dictionaries to swift-ime TSV format.
#
# Usage:
#   ./scripts/build_rime_dict.sh [output.tsv]
#
# Sources:
#   - rime-ice (雾凇拼音): https://github.com/iDvel/rime-ice
#     ~500k high-quality Chinese phrases with frequency data
#
# Output format (TSV, 3 columns): pinyin\tword\tweight
#   xiangshuibaiyang	香水白杨	10000
#   burongyixing	不容异性	100

set -euo pipefail
OUT="${1:-swift_ime_dict.tsv}"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "═══ Fetching RIME dictionary from rime-ice (雾凇拼音) ═══"
echo "→ cloning rime-ice…"
git clone --depth 1 https://github.com/iDvel/rime-ice.git "$TMPDIR/rime-ice" 2>/dev/null

echo "→ converting to TSV (3 columns: pinyin, word, weight)…"

convert_yaml() {
    local src="$1"
    # RIME format: word\tpinyin_with_tone\tweight\tcategory
    # Output: pinyin_without_tone\tword\tweight
    sed -n '/^\.\.\./,$ p' "$src" \
        | grep -v '^\.\.\.' \
        | grep -v '^#' \
        | grep -v '^$' \
        | awk -F'\t' '{
            gsub(/[0-9]/, "", $2);       # remove tone numbers from pinyin
            w = ($3+0 > 0) ? int($3) : 100;
            if (length($2)>0) print $2 "\t" $1 "\t" w;
        }' \
        | sort -u
}

# Core dictionary
if [ -f "$TMPDIR/rime-ice/cn_dicts/8105.dict.yaml" ]; then
    convert_yaml "$TMPDIR/rime-ice/cn_dicts/8105.dict.yaml" > "$TMPDIR/core.tsv"
fi

# Extended phrases
for f in "$TMPDIR/rime-ice/cn_dicts/"*.dict.yaml; do
    if [ -f "$f" ] && [ "$(basename "$f")" != "8105.dict.yaml" ]; then
        convert_yaml "$f" >> "$TMPDIR/core.tsv" 2>/dev/null || true
    fi
done

sort -u "$TMPDIR/core.tsv" -o "$OUT"
LINES=$(wc -l < "$OUT")
echo "→ done: $LINES entries written to $OUT"
echo ""
echo "To use in swift-ime:"
echo "  cargo run -p swift-ime -- --backend mock --dict $OUT"
