# 权重评价系统

> **状态: ✅ 已实现。** 2026-07-31 实测 Top-1 87.5%, Top-3 100%。

## 问题诊断（已修复）

### 根因

`fetch_dict.sh` 原来用 `cut -f1,2` 砍掉了 rime-ice YAML 的第 3 列（weight 字段）。
所有词进入 FST 时权重相同（occurrence count = 1），`freq_to_score()` 归一化后全部 0.25。

```
rime-ice YAML (3 列: word, pinyin, weight)
  │
  │ fetch_dict.sh OLD: cut -f1,2   ← 砍掉 weight
  │ fetch_dict.sh NEW: awk 保留 3 列
  ↓
rime-ice.tsv (3 列: pinyin, word, weight)
  │
  │ build_dict.rs: 读第 3 列 → DictBuilder.insert(pinyin, word, weight)
  ↓
FST binary (value = rime-ice weight, 100-500266)
  │
  │ lattice::freq_to_score(weight)
  ↓
差异化候选评分 ✅
```

## 评分公式

```
freq_score(weight) = log₂(weight + 1) / log₂(MAX_WEIGHT + 1)
  clamped to [0.25, 0.90]

where MAX_WEIGHT = 100_000
```

| weight | log₂(w+1) | 归一化 | clamp |
|--------|-----------|--------|-------|
| 500266 | 18.9 | 1.21 | **0.90** |
| 100000 | 16.6 | 1.06 | **0.90** |
| 10000  | 13.3 | 0.85 | **0.85** |
| 1000   | 10.0 | 0.64 | **0.64** |
| 100    | 6.7  | 0.43 | **0.43** |
| 1      | 1.0  | 0.06 | **0.25** |

评分实现在 `crates/ime-core/src/family/pinyin/lattice.rs:257-261`:
```rust
pub fn freq_to_score(freq: u64) -> f64 {
    let w = freq.max(1) as f64;
    let s = (w + 1.0).log2() / (Self::MAX_WEIGHT + 1.0).log2();
    s.clamp(0.25, 0.90)
}
```

## 当前数据源（统一 Lattice 后，5 个主 source）

| source | 数据源 | 评分 | 触发条件 |
|--------|--------|------|---------|
| `single` | inputx `dict.lookup()` | 1.0→0.4 | 有效单音节 |
| `lattice` | LatticeDecoder FST 全拼 | freq_to_score(weight) | 全拼精确匹配 |
| `lattice_mix` | LatticeDecoder 混写 | freq_to_score(weight) | 全拼+首字母混合 |
| `lattice_jp` | LatticeDecoder 简拼 | freq_to_score(weight) | 纯首字母 |
| `decomp` | Viterbi 分解 | 0.40 | 造词兜底 |
| `session` | inputx Session | 0.5 | 始终 |
| `phrase` | PhraseBook | 1.0 | 用户自造词置顶 |
| `prefix` | inputx `dict.prefix()` | 0.3 | 前缀兜底 |
| `phrase_prefix` | PhraseBook 前缀 | 0.85 | 用户词前缀 |

## 权重配置

所有参数可通过 `swift-ime.yaml` → `weights.pinyin` 节调整（`apps/swift-ime/config.rs`）：

```yaml
weights:
  pinyin:
    phrase_book: 1.0
    large_dict: 0.95
    viterbi_base: 0.3
    viterbi_scale: 0.65
    session: 0.5
    prefix: 0.3
    phrase_book_prefix: 0.85
    jianpin: 0.70
    single_syl_decay: 0.6
    context_boost: 0.15
    stopword_penalty: 0.5
    confirm_bonus: 0.05
    short_word_bonus: 0.02
    large_dict_take: 96
    viterbi_take: 48
    jianpin_take: 8
    prefix_take: 256
```

## 实测结果

测试用例 `assets/testcase/tc_draft.txt`（16 条），使用 rime-ice 900K 词条 + FST 权重：

```
═══════════════════════════════════
  Total:        16
  Top-1:        14  (87.5%)
  Top-3:        16  (100.0%)
  Top-10:       16  (100.0%)
═══════════════════════════════════
```

仅 2 条未达 Top-1：
- `jishi → 即使` (#2)
- `chushi → 初始` (#2)

## 数据流

```
rime-ice YAML (word, pinyin_with_tone, weight)
  │
  │ fetch_dict.sh: awk 去声调 + 保留 weight
  ↓
rime-ice.tsv (pinyin\tword\tweight)
  │
  │ build_dict.rs: DictBuilder.insert(pinyin, word, weight)
  ↓
rime-ice.fst (~50MB binary)
  │
  │ LatticeDecoder::new(fst)
  │   ├─ 构建 initials_index (O(n) 全表扫描, ~47s)
  │   └─ 缓存到 .fst.jianpin (~50ms 下次启动)
  ↓
predict(input)
  ├─ 全拼 → FST.get() O(1) → freq_to_score(weight)
  ├─ 简拼 → initials_index HashMap O(1) + pattern_match
  └─ 混写 → initials_index + pattern_match
```
