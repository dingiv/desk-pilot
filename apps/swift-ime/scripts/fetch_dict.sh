#!/usr/bin/env bash
# fetch_dict.sh — Download and convert RIME dictionaries to swift-ime TSV format.
#
# Usage:
#   ./scripts/fetch_dict.sh [output.tsv]
#
# Sources:
#   - rime-ice (雾凇拼音): https://github.com/iDvel/rime-ice
#     ~500k high-quality Chinese phrases with frequency data
#   - CustomPinyinDictionary: https://github.com/cloudskytian/CustomPinyinDictionaryForMSPinyinAndRime
#     ~2M entries (larger but more noise)
#
# Output format (TSV): pinyin\tword
#   xiangshuibaiyang\t香水白杨
#   burongyixing\t不容异性

set -euo pipefail
OUT="${1:-swift_ime_dict.tsv}"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "═══ Fetching RIME dictionary from rime-ice (雾凇拼音) ═══"
echo "→ cloning rime-ice…"
git clone --depth 1 https://github.com/iDvel/rime-ice.git "$TMPDIR/rime-ice" 2>/dev/null

# The main dictionary files are in cn_dicts/
# Format: word\tsyllable\tfrequency\tcategory
# We extract word + pinyin (no tones), swap to pinyin\tword
echo "→ converting to TSV…"

convert_yaml() {
    local src="$1"
    # RIME format: hanzi\tpinyin_with_tone\tfreq\tcategory
    # Strip the YAML header and extract columns 1 & 2, strip tones from pinyin
    sed -n '/^\.\.\./,$ p' "$src" \
        | grep -v '^\.\.\.' \
        | grep -v '^#' \
        | grep -v '^$' \
        | cut -f1,2 \
        | sed 's/[0-9]//g' \
        | awk -F'\t' '{ if (NF==2 && length($2)>0) print $2 "\t" $1 }' \
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
