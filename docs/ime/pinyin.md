# 拼音家族架构

> 最后更新: 2026-07-31

## 当前 Member（5 个数据源，统一 Lattice）

原 8 个 member 中的 `dict` + `viterbi` + `jianpin` 已合并为 `LatticeDecoder`。

| source | 数据源 | 基础分 | 触发条件 | 作用 |
|--------|--------|--------|---------|------|
| `single` | inputx `dict.lookup()` | 1.0→0.4 | 有效单音节 | L1 频率排序 |
| `lattice` | LatticeDecoder FST 全拼 | freq_to_score(weight) | 全拼精确匹配 | 主力 |
| `lattice_mix` | LatticeDecoder 混写 | freq_to_score(weight) | 全拼+首字母混合 | gyinsjian→光阴似箭 |
| `lattice_jp` | LatticeDecoder 简拼 | freq_to_score(weight) | 纯首字母 | gysj→光阴似箭 |
| `decomp` | Viterbi 分解 | 0.40 | 多音节兜底 | 造词 |
| `phrase` | PhraseBook 全拼 | 1.0 | 用户自造词 | 置顶 |
| `phrase_sp` | PhraseBook 声母 | 0.95 | 用户词简拼 | lzm→李正明 |

### 查询顺序

```
single-syllable:  single → phrase + phrase_sp
multi-syllable:   lattice (全拼/混写/简拼) → decomp (Viterbi fallback) → phrase + phrase_sp
```

## LatticeDecoder 统一引擎

替代了原来的 `dict` (LargeDict exact)、`viterbi` (bigram composition)、`jianpin` (initials index) 三个独立 member。

### 算法: 声母边界分段 + 首字母快查 + 模式校验

```
Input: "guangyinsj"

1. greedy_parse("guangyinsj") → [Full("guang"), Full("yin"), Full("s"?), Initial('j')]
   实际: 逐位取最长有效音节; s 不是有效音节 → [Full("guang"), Full("yin"), Initial('s'), Initial('j')]

2. 段首字母: "gysj"

3. initials_index["gysj"] → [(guangyinsijian, 光阴似箭, 1200), ...]

4. pattern_match: 逐段校验
   guang(g) + yin + si(s) + jian ✓ → 光阴似箭
```

### 匹配类型

| 类型 | 条件 | 评分 |
|------|------|------|
| `Full` | 所有段都是完整音节 | freq_to_score(weight) |
| `Mixed` | 部分 Full + 部分 Initial | freq_to_score(weight) |
| `Initials` | 全部是首字母 | freq_to_score(weight) |

### 性能

- 全拼路径: `FST.get()` O(1) 查找，极快
- 简拼/混写路径: `initials_index` HashMap O(1) + `pattern_match` 逐条校验
- 启动: 首次构建 initials_index ~47s（全表扫描 900K 词条），之后读 `.fst.idx` 缓存 ~50ms

### 数据

- rime-ice 900K 词条，3 列 TSV（pinyin, word, weight）
- `build_dict.rs` 编译为 inputx_fsa FST
- 权重范围 100-500266，`freq_to_score` log₂ 归一化到 [0.25, 0.90]


### 模糊音支持


### 拼写纠偏功能

qiangzhuang

qinagzhaung



