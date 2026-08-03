# swift-ime 第三轮 — 任务清单

> 创建: 2026-07-31

---

## 1. 用户自定义词典 FST

### 动机

当前只加载一个 `rime-ice.fst`。用户可能有自己的词典（专业术语、人名、公司名），希望像 rime-ice 一样享受 FST 频率排序 + 简拼/混写。

### 设计

```
assets/dict/
  rime-ice.fst       ← 系统词典 (900K)
  rime-ice.fst.idx   ← 系统词典 initials 缓存
  my_words.fst       ← 用户词典 (自定义)
  my_words.fst.idx   ← 用户词典 initials 缓存
```

**处理方式**：和 `rime-ice.fst` 完全相同。

- 文件放在 `assets/dict/` 目录下，以 `.fst` 后缀命名
- 启动时 `PinyinFamily` 遍历所有 `.fst` 文件，各自构建 `LatticeDecoder` + `.idx` 缓存
- 查询时合并多个 lattice 的结果，按 `freq_to_score(weight)` 统一排序
- 用户词典的词可以和 rime-ice 的词竞争（同频同权）

### 数据流

```
多个 .fst 文件
  │
  ├─ rime-ice.fst   → LatticeDecoder A  (initials_index A,  freq_to_score)
  ├─ my_words.fst   → LatticeDecoder B  (initials_index B,  freq_to_score)
  └─ company.fst    → LatticeDecoder C  (initials_index C,  freq_to_score)
        │
        └─ predict(input):
             ├─ 每个 decoder 独立查询
             ├─ 合并结果，按 freq_score 降序
             └─ truncate(max_results)
```

### 构建工具扩展

`build_dict.rs` 已经是通用工具——任何 `pinyin\tword\tweight` TSV 都可以编译为 FST：
```bash
cargo run --bin build_dict -- my_words.tsv my_words.fst
```

用户只需准备 TSV 文件，运行一次即可。

### 关键问题

- **多 FST 的去重**：同一个词可能出现在多个 FST 中，取最高频
- **initials_index 合并**：多个 decoder 的 initials_index 独立查询，结果合并后按 freq 排序
- **缓存策略**：每个 `.fst` 独立 `.idx`，各自检查时效性

---

## 2. RecentFamily — 近期输入家族

### 动机

当前所有预测都基于拼音。但用户经常需要重复输入刚打过的词（中英文都有）。这些词应该不依赖拼音直接出现在候选列表前面。

### 设计

```
RecentFamily (priority=90, 仅次于 PinyinFamily=100)
  │
  ├─ 最近使用 (recency)  — 最近 N 次 commit 的词，按时间衰减
  └─ 最常使用 (frequency) — 历史 commit 次数最多的词，按次数排序
```

**数据结构**：

```rust
struct RecentStore {
    // 环形缓冲区：最近 64 次 commit
    recency: VecDeque<String>,
    // 全局计数：word → 总使用次数
    frequency: HashMap<String, u32>,
}
```

**评分规则**：

| 来源 | 评分 | 说明 |
|------|------|------|
| 刚 commit 的词 | 0.95 | 最近 3 次 |
| 最近 10 次 | 0.85 | 衰减 |
| 最近 64 次 | 0.70 | 更衰减 |
| 高频词（Top-20）| 0.60 | 历史使用超过 10 次 |
| 高频词（Top-100）| 0.50 | 历史使用超过 5 次 |

**触发条件**：始终激活。输入任意字符时，从 `RecentStore` 取出匹配前缀的词。

- 用户输入 "h" → 最近用过 "hello"、"你好" → 候选中出现
- 用户输入 "bl" → 最近用过 "black" → 候选中出现
- 用户输入任意拼音 → 最近用过对应的中文词 → 候选中出现

### 与 PinyinFamily 的关系

RecentFamily 不分中英文，只看用户实际 commit 过的文本。它通过 `UnifiedScorer` 和 PinyinFamily 竞争：

```
输入 "bl"
  ├─ PinyinFamily  → lattice 无结果（"bl" 不是拼音）
  ├─ EnglishFamily → "black" (prefix match)
  └─ RecentFamily  → "black" (recency boost, 用过所以排第一)
```

### 持久化

走 SQLite `WeightStore`，新增 `recent_words` 表：

```sql
CREATE TABLE recent_words (
    word TEXT NOT NULL PRIMARY KEY,
    count INTEGER NOT NULL DEFAULT 1,
    last_used INTEGER NOT NULL DEFAULT (unixepoch())
);
```

- 每次 commit 时 `UPSERT`，`count+1`，更新 `last_used`
- 启动时 `load_all_recent()` → warm 进 `RecentStore`

---

## 3. Phrase 声母分词

### 动机

用户造词 "李正明" (lizhengming)。下次输入全拼 `lizhengming` 可以 recall ✅。但输入简拼 `lzm` 找不到 ❌。

### 设计

在 `PhraseBook::insert()` 时，同时计算声母串并建立反向索引：

```
用户造词: "李正明" (pinyin="lizhengming")

1. inputx 分词: "lizhengming" → ["li", "zheng", "ming"]
2. 提取声母: "lzm"
3. 存入 PhraseBook:
     entries["lizhengming"] → ["李正明"]        (全拼, 已有)
     initials_index["lzm"]  → ["李正明"]        (声母, 新增)
```

**查询时**：`PhraseBook` 新增 `by_initials(initials: &str) -> Vec<String>`。当用户输入 `lzm`，PinyinFamily 的 `predict()` 在 phrase 阶段不仅查 `book.exact(input)`，也查 `book.by_initials(input)`。

### 触发位置

在 PinyinFamily 的 `predict()` 中，紧挨 phrase 精确查询之后：

```rust
// ── PhraseBook: exact match ──
for w in book.exact(input) { ... }   // "lizhengming" → "李正明"

// ── PhraseBook: initials match (NEW) ──
for w in book.by_initials(input) {   // "lzm" → "李正明"
    if !out.iter().any(|c| c.text == w) {
        out.push(ScoredCandidate {
            text: w, family: "pinyin", source: "phrase_sp",
            raw_score: 0.95,  // 略低于 exact phrase (1.0)
        });
    }
}
```

### 与 LatticeDecoder 的关系

LatticeDecoder 已经能用简拼匹配 rime-ice 词条（`gysj → 光阴似箭`）。但用户自造词不在 rime-ice 里，所以 LatticeDecoder 的 initials_index 覆盖不到。Phrase 声母索引填补的就是这个空白。

### 数据模型扩展

```rust
pub struct PhraseBook {
    entries: HashMap<String, Vec<Phrase>>,        // 全拼索引 (已有)
    initials_index: HashMap<String, Vec<Phrase>>,  // 声母索引 (新增)
}
```

`insert()` 时双写，`by_initials()` 时查 `initials_index`。

### 持久化

声母索引是派生数据——从 `entries` 可以完全重建。所以 SQLite 只需存 `entries`（已有），启动 warm 后重建 `initials_index`。

---

## 📋 行动计划

| 优先级 | 任务 | 规模 | 依赖 |
|--------|------|------|------|
| P1 | Phrase 声母分词 | ✅ 已完成 | — |
| P1 | RecentFamily | 中 (~200行) | WeightStore 扩展 |
| P2 | 用户自定义词典 FST | 中 (~150行) | 无 |
