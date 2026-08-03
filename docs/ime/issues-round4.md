# swift-ime 第四轮 — 任务清单

> 创建: 2026-08-03

---

## 1. 上下文感知预测

### 动机

当前引擎只在 commit 时记录 `last_word` 到 `InputContext`，PinyinFamily 用它做 bigram boost。但实际用户场景中：

- 刚打过的词（"李正明"）第二次打时应该直接出现在候选第一位，不需要重走全拼→Viterbi→造词
- "大陆"打完再打"陆" → bigram 已闭合 ✅，但这种上下文仅限相邻两个词
- fcitx5 框架提供了 `surroundingText()` 回调——光标前后真实文本，当前未接入

### 设计

#### 1a. Short-term recency cache

```
最近 64 次 commit 的词 → 按时间衰减

  刚 commit (最新 3 次):  0.95 → 0.85 → 0.75
  最近 10 次:            0.60
  最近 64 次:            0.40
```

**数据结构**：

```rust
struct RecencyStore {
    // 环形缓冲：最近 64 次 commit 的词
    recency: VecDeque<String>,
}

impl RecencyStore {
    fn push(&mut self, word: &str);       // commit 时调用
    fn boost(&self, word: &str) -> f64;   // predict 时查询
}
```

**与 PinyinFamily 集成**：

在 `predict_with_context()` 中，先查 recency，再查 bigram：

```rust
for c in &mut candidates {
    // Layer 1: recency boost
    let recency_boost = recency.boost(&c.text);
    // Layer 2: bigram boost
    let bigram_boost = bigram.boost(&ctx_words, &c.text);
    // Merge
    c.raw_score = (c.raw_score + recency_boost + bigram_boost).min(1.0);
}
```

**持久化**：走 SQLite `WeightStore`，新增 `recency_words` 表。每次 commit 时 `UPSERT`；启动时 warm。

#### 1b. Wire fcitx5 surrounding text

fcitx5 在 `InputMethodEngineV3` 中提供了：

```cpp
void surroundingTextCallback(
    InputContext *ic,
    const std::string &text,  // 光标前文本
    int cursor, int anchor
);
```

**接入方案**：

1. C++ glue engine.cpp 实现 `surroundingTextCallback`
2. 通过 C ABI `swift_ime_set_surrounding(ctx, text)` 传给 Rust
3. ImeEngine 在 predict 前检查 surrounding text，有则更新 `InputContext`

```rust
// InputContext 扩展
pub struct InputContext {
    pub recent_text: String,      // 已 commit 的内部记录 (20 chars)
    pub last_word: String,         // 上一个词
    pub surrounding: String,       // NEW: fcitx5 报告的光标前文本
}
```

**使用**：surrounding 提供比 recent_text 更长的上下文窗口（光标前 ~50 chars）。用它做 bigram/trigram 查找，覆盖"大陆"→"陆"这种已被内部记录覆盖的场景之外的新上下文。

#### 1c. 扩展到 EnglishFamily

EnglishFamily 当前只做前缀匹配，不接受上下文。添加 `predict_with_context()`：

```rust
fn predict_with_context(&self, input: &str, ctx: &InputContext) -> Vec<ScoredCandidate> {
    let mut candidates = self.predict(input);
    // 如果上下文最后一个词是英文 → boost 英文候选
    if ctx.last_word.chars().any(|c| c.is_ascii_alphabetic()) {
        for c in &mut candidates {
            c.raw_score = (c.raw_score + 0.10).min(1.0);
        }
    }
    candidates
}
```

### 数据流

```
fcitx5 surroundingTextCallback
  → swift_ime_set_surrounding(text)
    → ImeEngine::set_surrounding(ctx, text)
      → InputContext.surrounding = text

用户键入拼音
  → Dispatcher::process_key
    → UnifiedScorer::rank_with_context(input, &InputContext {
        recent_text: "大陆",
        last_word: "大陆",
        surrounding: "中国的",
      })
      → PinyinFamily::predict_with_context
        → recency.boost(candidates)     // "大陆" 刚打过 → boost
        → bigram.boost("大陆", "陆")     // 已知 bigram → boost
        → surrounding context            // "中国的大陆" → boost "大"
```

---

## 2. 中英文混合预测

### 动机

当前 `PinyinFamily(100)` + `EnglishFamily(60)` 在 `UnifiedScorer` 中合并。英文只有 200 个硬编码词。实际使用中：

- 输入 "api" → PinyinFamily 尝试分解 "a pi" → 垃圾候选；EnglishFamily 有 "about/after/..." → 但不含 "api"
- 输入 "hello" → EnglishFamily 能匹配 "help"，但 200 词的覆盖极低
- 理想：像 macOS 中文输入法一样，"api" → "API" 出现在候选，"hello" → "hello" 出现在候选

### 设计

#### 2a. 英文词库 FST

和 rime-ice 完全相同的路径：`TSV → build_dict → .fst`。

**数据源**：`google-10000-english-no-swears.txt` (~10K words, public domain)

```
格式 (TSV, 2 columns): word\tfreq
  the     10000
  of      9800
  and     9500
  ...
```

**编译**：
```bash
# 将 freq 转为 weight 列
awk '{print $1 "\t" $1 "\t" $2}' words.txt > en_words.tsv
cargo run --bin build_dict -- en_words.tsv en_words.fst
```

**加载**：`EnglishFamily` 启动时 `LatticeDecoder::new(en_dict, "en_words.fst")`，享受 O(1) FST 查找 + freq_to_score。

**LatticeDecoder 适配**：当前 `LatticeDecoder` 为拼音设计（`get(pinyin)` → words）。英文场景 `get("hel")` 需要前缀匹配。方案：

- 全量加载 FST 到 `HashMap<String, Vec<(String, u32)>>`（10K 词 ~2MB 内存）
- 前缀匹配走 `HashMap` 遍历（10K 条 ~0.2ms）
- 或者扩展 `LatticeDecoder` 支持前缀模式

**简化方案**：不引入 FST。保持当前的前缀匹配，但用更大的词表（`BTreeMap` 或 `Vec` + binary search）。10K 词的线性扫描 ~0.05ms，可接受。

#### 2b. 智能路由

在 `UnifiedScorer` 中根据输入特征动态调整各家族权重：

```rust
fn effective_priority(&self, input: &str) -> u32 {
    if inputx_pinyin::is_valid_syllable(input.chars()) {
        // 纯拼音 → 中文优先
        self.priority()
    } else {
        // 非拼音 (含数字/特殊字符) → 降权
        self.priority() / 2
    }
}
```

或者更简单：在 EnglishFamily 的 priority 上浮：

| 输入特征 | PinyinFamily 权重 | EnglishFamily 权重 |
|---------|-------------------|-------------------|
| 纯拼音合法序列 ("nihao") | 100 | 60 |
| 非拼音 ("hello", "api") | 50 | 85 |
| 拼音前缀模糊 ("xi") | 100 | 60 |

**判定函数**：
```rust
fn input_class(input: &str) -> InputClass {
    if input.is_empty() { return InputClass::Empty; }
    // 全 ascii 小写 → 检查是否都是合法拼音音节
    if input.chars().all(|c| c.is_ascii_lowercase()) {
        // 任意子串都是合法拼音 → pinyin-like
        if is_pinyin_like(input) { InputClass::PinyinLike }
        else { InputClass::EnglishLike }
    } else {
        InputClass::Mixed
    }
}
```

#### 2c. 英文上下文

EnglishFamily 也实现 `predict_with_context()`：

```rust
fn predict_with_context(&self, input: &str, ctx: &InputContext) -> Vec<ScoredCandidate> {
    let mut candidates = self.predict(input);
    // 上下文是英文 → boost
    let ctx_is_english = ctx.last_word.chars()
        .any(|c| c.is_ascii_alphabetic());
    if ctx_is_english {
        for c in &mut candidates {
            c.raw_score = (c.raw_score + 0.15).min(1.0);
        }
    }
    candidates
}
```

---

## 📋 行动计划

| 优先级 | 任务 | 规模 | 说明 |
|--------|------|------|------|
| **P0** | 1a. Recency cache | ~120行 | 最近 64 词的衰减加权，最大用户体验提升 |
| P0 | 1b. Wire surrounding text | ~60行 | fcitx5 C++ → C ABI → Rust InputContext |
| P1 | 1c. English context | ~40行 | EnglishFamily 接入上下文感知 |
| P1 | 2a. 英文词库升级 | ~100行 | 10K 英文词 → 前缀匹配，替换 200 词硬编码 |
| P1 | 2b. 智能路由 | ~80行 | 检测拼音/英文，动态调整家族权重 |
| P2 | 2c. 英文上下文 | ~50行 | EnglishFamily predict_with_context |

**推荐执行顺序**：1a → 1b → 1c → 2a → 2b → 2c

---

### 与现有 issue 的关系

| 新任务 | 对应旧任务 | 差异 |
|--------|-----------|------|
| 1a Recency | round3 #2 `RecentFamily` | 更轻量：只做 recency(衰减)，不做 frequency(计数)；直接放在 PinyinFamily 内部而非独立家族 |
| 1b Surrounding | — | 完全新增 |
| 2a English FST | — | 完全新增 |
| 2b Smart routing | — | 完全新增 |
