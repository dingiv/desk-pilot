# 权重评价系统

> **状态: ✅ 已实现(2026-08-10 大改版)。** 打分参数全部可配(`swift-ime.yaml` →
> `weights`);调试模式(`debug.candidate_meta`)可在候选词后直接显示
> `[score family/source]`。

## 总览:三层打分架构

```
候选原始分 (family 内部, 0.0~1.0)
  × 家族优先级/100 (pinyin 100 / english 70 / emoji 60)
  + 叠加层(recent member 权重合成、short_word_bonus)
  → 全局排序
```

`#`(magic)与 `/`(snippet)强制前缀分流,候选由 FSM 直填,**不参与**统一打分。

---

## 1. 词典词分数链(rime-ice lattice)

### 词频归一化

```
freq_to_score(f) = log₂(f+1) / log₂(max_freq+1)   clamp [0.25, 1.0]
```

- `max_freq` = **构建索引时记录的实际最大词频**(rime-ice ≈ 501,276),随
  cache v2 头持久化 —— 映射对齐真实分布,高频词不会饱和成同分。
- 可通过 `weights.freq_scale.max_weight` 显式覆盖(`0` = auto,默认)。

(线性重标到 [0.25, 0.90],非 clamp —— 保持严格单调,顶流封顶留加成空间)

| 词频 | 分数 | 实例 |
|---|---|---|
| 501,276 | 0.900 | 继续(顶流封顶) |
| 164,505 | 0.845 | 急须 |
| 73,750 | 0.775 | 积蓄 |
| 22,600 | 0.687 | 几许 |
| 12,495 | 0.644 | 记叙 |

### 匹配类型折扣

| source | 触发 | 分数 |
|---|---|---|
| `lattice` | 全拼精确 | freq 分 × 1.0 |
| `lattice_mix` | 全拼+声母混写 | freq 分 × `jianpin`(0.50) |
| `lattice_jp` | 纯简拼(nh→你好) | freq 分 × `jianpin`(0.50) |
| `lattice_prefix` | **前缀联想**(naozh→闹钟) | freq 分 × `prefix_lookup`(0.75) × 距离衰减 |
| `single` | 单音节(de→的) | **`large_dict` 为基础分**,按位衰减 × `single_syl_decay`(注意:`large_dict` 不是死参数——它正是单字候选的基础分) |
| `decomp` | Viterbi 造词兜底 | 0.40 |

### 前缀联想距离衰减(`scoring::prefix_decay`,pinyin/emoji 共享)

联想词拼音比输入长越多越不可信:剩余 **≤3 字符免费**(覆盖"半截声母到
完整音节"的典型差,zh→zhong 差 3),超出按 `0.85^超出` 衰减。作用:
`jix→jixiaokao(差6)` 这类宽前缀捞到的高频长词沉底,不淹没
`jix→继续(差1)`。english 的前缀是**质量式**(0.60 地板 + 0.25×词频×
匹配率,无距离项)—— 语义不同,不共享此 helper。

### 为什么需要前缀联想

`greedy_parse` 切不开"半截声母":`naozh` 中 `zh` 非法音节 → 拆 `z+h` 两段
→ 输入 3 段 vs `naozhong` 2 音节 → `pattern_match` 永远 false。FST 原生
前缀遍历(`predict_prefix`,1024 条收集池)补上这个洞。

---

## 2. 自生词分数链(PhraseBook 单词本)

### count 曲线

```
phrase_score(count) = min(0.70 + 0.02 × (count-1), 0.88)
```

| 使用次数 | 分数 | 等价词典词频 | 词典中的位置 |
|---|---|---|---|
| 1(刚造) | 0.700 | ~9,800 | 中等偏冷 |
| 5 | 0.780 | ~27,900 | 中频 |
| 10+(封顶) | 0.880 | ~104,000 | 高频 |
| — 对照 | 0.900 | 501,276 | 顶流(继续,封顶)|

**设计意图**:新造词从"中等偏冷词典词"起步,用 10 次升到"高频词典词",
封顶 0.88 永远够不到词典顶流 —— 高频学习不霸榜。

声母召回(lzm→李正明):phrase 分 × 0.95。

### 两条学习路径

| 路径 | 判定 | 学习? |
|---|---|---|
| 直接提交(空格选 top) | `committed_text` 为空 | ❌ **不学**——decomp 词 Viterbi 下次重新组合,无需入本(qingqiuti→请求提 回归) |
| 自生词流程(数字键逐字选) | 经历 ≥1 次 partial commit | ✅ 唯一学习入口,无条件入本(主动造词) |

学习入口只收 **汉字+ASCII 字母数字** 组成的词("Bevy引擎" ✓,📀 ✗)——
emoji 提交不进拼音单词本(它们会吃 phrase+recent 双重加成霸榜)。
存量污染在 warm 时过滤,无需清库。

---

## 3. 自生词 × 词典词关系(2026-08-10 分析)

### 实测交叉点

| 场景 | 自生词 | 词典词 | 判定 |
|---|---|---|---|
| 全拼 `lizhengming` | 李正明 0.700(phrase) | decomp 0.400 | ✓ 造词召回是刚需 |
| 简拼 `nh` | 你好 0.710(phrase 声母) | 女孩 0.503(lattice_jp) | ⚠️ P1 |
| recent 后(b=5) | c≥10: 0.88→0.968 | 0.852 词→0.960 | ⚠️ P2 |
| 全拼封顶 | phrase ≤0.88 | 顶流 1.0 | ✓ P3 保护 |

### 已知问题(权衡,未修)

- **P1 简拼压制过强(+0.167)**:词典简拼打 0.5 折,自生词声母只打 0.95 折。
  首次学习的自生词(c=1)在简拼下就压过词典顶流。可调:声母折扣 0.95→0.80。
- **P2 recent 叠加后反超**:c≥10 自生词+recent(0.968)> 词典 0.852 词+recent
  (0.960)。强用户偏好压过词典高频,勉强合理;根因是 recent 对两者同权。
- **P3 全拼封顶隔离(保护,合理)**:词典顶流(的/了/继续级)始终安全。

---

## 4. 叠加层

### Recent member(近期指数权重合成)

提交时记录词 + wall-clock 时间戳;候选再次出现时按距上次使用分档:

| 距上次使用 | 近期指数 b |
|---|---|
| ≤10s | 5 |
| ≤1h | 4 |
| ≤5h | 3 |
| ≤1d | 2 |
| ≤3d | 1 |
| >3d | 移出 |

```
z = (1-a) × (a+b) / 8 + a        a = 候选原权重, b = 近期指数
```

性质:增量与 (1-a) 成比例 —— 低权重词获更大加成、高权重词增量趋零,
**z 天然 < 1,不会顶满**(取代旧的"加固定值再 min(1.0)"做法)。

| a \ b | 1 | 5 |
|---|---|---|
| 0.70 | 0.764 | 0.914 |
| 0.88 | 0.892 | 0.968 |

### 前缀整词联想(上下文感知)

提交词的拼音 + 当前输入拼音 → 拼接查词典 → 整词以上一词开头的,**尾字**
以**整词权重**提升(source `context_comp`)。例:提交 是(shi)后输入 de →
`shide`→是的(350,380)→ 的 提升至 0.960。整词权重低于候选现分则不动
(只升不降)。开关:`input.context_aware`。

### 其他

- `short_word_bonus`(0.01):2 字词加成
- `stopword_penalty`(0.5):全虚词组合折扣

---

## 5. 其他家族关键规则

**emoji**(优先级 60):
- exact 完整命中 1.0;前缀 0.6 + `prefix_decay` 距离衰减(剩余 ≤3 免费)
- **≤2 字母关键词(cd→📀、ok→👍)即使完整命中也降为前缀档** —— 两字母
  输入几乎总是中文简拼或 ASCII 缩写
- 关键词表 = 内置 28 + CLDR 生成(`emoji.tsv`,英文+**拼音**关键词,汉字
  关键词无法在拼音 buffer 触发所以转换) + 用户表

**english**(优先级 70):exact 0.88,prefix 按匹配比例(≤0.6)。

---

## 6. 全部可配参数(swift-ime.yaml → weights)

```yaml
weights:
  family_priority:
    pinyin: 100
    english: 70
    emoji: 60
  pinyin:
    phrase_book: 0.88       # 自生词封顶
    large_dict: 0.85
    jianpin: 0.50           # 混写/简拼折扣
    prefix_lookup: 0.75     # 前缀联想折扣(naozh→闹钟)
    single_syl_decay: 0.5
    short_word_bonus: 0.01
    # …(viterbi/context/stopword 等见 yaml 注释)
  freq_scale:
    max_weight: 0           # 0=auto(实际最大词频), >0 显式分母
    min_score: 0.25
    max_score: 0.90
```

固定不可配:recent 合成公式、前缀/emoji 距离衰减(0.85 底数)、
emoji ≤2 字母降权、phrase count 曲线斜率(0.02/次,起点 0.70)。

## 7. 调试

```yaml
debug:
  candidate_meta: true   # fcitx 候选词右侧显示 [score family/source]
```

TUI 始终显示;mock `--verbose` 同样输出。

---

## 数据流

```
rime-ice YAML (word, pinyin, weight)
  │ fetch_dict.sh: awk 去声调 + 保留 weight
  ↓
rime-ice.tsv → build_dict.rs → rime-ice.fst (~22MB)
  ↓
LatticeDecoder::new(fst)
  ├─ initials_index 构建(~46s 首次)→ cache v2(记录实际 max_freq)→ DATA:: 命名空间
  ↓
predict(input)
  ├─ 全拼 → FST.get() O(1) → freq_to_score
  ├─ 简拼/混写 → initials_index + pattern_match × jianpin
  └─ 前缀联想 → FST.prefix_for_each × prefix_lookup × 距离衰减
```

## 历史基线

2026-07-31(rime-ice 权重修复后):Top-1 87.5%, Top-3 100%
(用例 `assets/testcase/tc_draft.txt`,16 条;`jishi→即使`、`chushi→初始` #2)。
