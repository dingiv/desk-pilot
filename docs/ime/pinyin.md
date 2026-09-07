# 拼音家族架构

> 最后更新: round19(2026-09-07),与代码同步。
> 打分细节见 [weight-scoring.md](weight-scoring.md);overlay 见
> [overlay.md](overlay.md)。

## Member / 候选来源(source)

原 8 个 member 中的 `dict` + `viterbi` + `jianpin` 已合并为
`LatticeDecoder`(`family/pinyin/lattice.rs`)。

| source | 数据源 | 基础分 | 触发条件 |
|--------|--------|--------|---------|
| `lattice` | LatticeDecoder FST 全拼(含 overlay 旁路词条) | `freq_to_score(weight)` | 全拼精确匹配 |
| `lattice_mix` | LatticeDecoder 混写 | freq 分 × `jianpin`(0.50) | 全拼+首字母混合 gyinsjian→光阴似箭 |
| `lattice_jp` | LatticeDecoder 简拼 | freq 分 × `jianpin`(0.50) | 纯首字母 gysj→光阴似箭 |
| `lattice_prefix` | FST 前缀遍历 | freq 分 × `prefix_lookup`(0.75) × `prefix_decay` 距离衰减 | 前缀联想 naozh→闹钟;存在 Full 精确命中时再折一次(W4) |
| `single` | inputx `dict.lookup()` | `freq_to_score(单字词频)`(W1 词频驱动) | 有效单音节;内嵌词典序仅兜底 |
| `decomp` | Viterbi 分解 | `viterbi_base`(0.40)+scale,带 [0.40,0.45](E4) | 多音节兜底造词 |
| `phrase` | PhraseBook 全拼 | `phrase_base`(0.70)+`phrase_step`(0.02/次),封顶 `phrase_book`(0.88) | 用户自造词 |
| `phrase_sp` | PhraseBook 声母 | phrase 分 × `phrase_initials_ratio`(0.95) | 用户词简拼 lzm→李正明 |
| `overlay` | Overlay 三级词典(round19) | `freq_to_score(有效频率)` | MergedDict 覆盖 + L1/L2 独立出候选(上限 8);种子词继承原频率 → 排序兼容 |
| `chain` | 链式预测缓存(round15) | 缓存基线分继承 | `abc'#asr'` 场景上游段缓存命中 |
| `context_comp` | 整词联想(上下文感知) | 整词权重(只升不降) | 上次提交词拼音 + 当前输入拼接查词典 |

## 查询顺序

```
single-syllable:  single → phrase + phrase_sp
multi-syllable:   lattice (全拼/混写/简拼/前缀) → decomp (Viterbi fallback) → phrase + phrase_sp
收尾:             MergedDict overlay 三级覆盖(L1 > L2 > L3)+ 独立 overlay 候选
微调:             tier(recency + 频次增强)、bigram、整词联想 → 全局排序
```

## LatticeDecoder 统一引擎

### 算法: 声母边界分段 + 首字母快查 + 模式校验

```
Input: "guangyinsj"

1. greedy_parse → [Full("guang"), Full("yin"), Initial('s'), Initial('j')]
2. 段首字母: "gysj"
3. initials_index["gysj"] → [(guangyinsijian, 光阴似箭, 1200), ...]
4. pattern_match: guang(g) + yin + si(s) + jian ✓ → 光阴似箭
```

### 匹配类型

| 类型 | 条件 | 评分 |
|------|------|------|
| `Full` | 所有段都是完整音节 | freq_to_score(weight) |
| `Mixed` | 部分 Full + 部分 Initial | freq_to_score(weight) |
| `Initials` | 全部是首字母 | freq_to_score(weight) |

### Overlay 旁路词典(round19)

LatticeDecoder 另挂 overlay 旁路(`set_overlay_entries`):自生词/L1/L2
词条以生成号驱动同步进五个查询点(全拼/前缀/声母对齐),与 FST 词条
同场竞争,同词双册时 overlay 有效频率胜出。详见 [overlay.md](overlay.md) §4.2。

### 性能

- 全拼路径: `FST.get()` O(1)
- 简拼/混写路径: `initials_index` HashMap O(1) + `pattern_match` 逐条校验
- 启动: 首次构建 initials_index ~47s(全表扫描),之后读 cache(~50ms);
  overlay 同步按 generation 比对,变了才重灌

### 数据

- SeedDict: rime-ice 91.6 万词条,3 列 TSV(pinyin, word, weight)→
  `build_dict.rs` 编译为 FST(~22MB);权重范围 10 万级,
  `freq_to_score` log₂ 归一化到 [0.25, 0.90]
- 词典数据质量问题(两种量纲混用)与清洗计划见 [overlay.md](overlay.md) §七

## 学习与记忆

- **L0 用户模型**:留在 inputx_pinyin 引擎词典内,经家族句柄
  `record_pick` 写(边界例外)
- **bigram 上下文**:`last_commit`(record_pick 写入)→ `bigram_boost`
- **提交统计**:家族不感知 commit;stage2 产出 CommitReceipt,由
  ControlPane 统一 `post::learn_commit` 结算 → WordBook/MemoryLayer
  (见 [overlay.md](overlay.md) §五)
