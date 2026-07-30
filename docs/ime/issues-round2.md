# swift-ime 第二轮系统优化 — 问题清单

## 1. 候选词数量不足

**现象**: 同一全拼（如 `jishi`），fcitx5-chinese-addons 给出 20+ 候选项，swift-ime 仅 8-16 个。

**根因**:
- rime-ice（900K）虽然数据量大，但结构松散，大量是 3+字长尾词条
- inputx-pinyin 内嵌词典仅 3.9MB（~15 万条），覆盖面有限
- 缺少权威的 2-3 字高频词词典

**方向**:
- 引入更多权威词典源（如 THUOCL 清华大学开放中文词库、Sogou 细胞词库等）
- 将词典预处理为统一 TSV 格式 → LargeDict 加载
- 或使用 inputx-fsa 的 `DictBuilder` 将外部词典编译为 FST 供 inputx 原生查询

## 2. 造词系统权重偏低

**现象**: 用户逐字组词（如 `李→正→明` → `李正明`）后，该词仅通过 PhraseBook 保存，下次输入时虽有 1.0 分但无上下文区分度。

**方向**:
- 造词完成后应同时更新：PhraseBook（已做）、inputx L0 `pin()`（如果词在 L1）、LargeDict 频率 +1
- 造词的每个中间步骤（选中 `李`、`正`、`明`）也应记录为 L0 pick
- 造词成品应获得比普通选中更高的基础权重

## 3. 预测数量限制 + 惰性加载

**现象**: 所有家族一次性生成全部候选词，被 `top_n()` 和 `take(N)` 截断。用户翻到第 2 页后无更多内容。

**方向**:
- 每个家族初始只生成前 N 个（凑够 2-3 页，约 32-48 个）
- `CandidateFamily` 新增 `predict_more(input, offset, limit)` 方法
- 当 fcitx5 翻页事件触发时，C++ 侧调用新的 C ABI 获取更多候选
- 需要 C ABI 新增 `swift_ime_predict_more(ctx, page)` 接口

## 4. 翻页后空格提交错误候选

**现象**: 用户翻到第 2 页，目标词在第 2 页第一位高亮。按空格，提交的是第 1 页第一位。

**根因**: fcitx5 C++ `keyEvent` 中，空格键的处理是：
```
1. 选首候选 → 空格触发 select_candidate(0)
```
翻页后 `candidate_highlight` 未同步到 Rust 侧，Rust 的 `select()` 仍用 `candidate_highlight = 0`。

**方向**:
- C++ 翻页（PageDown）时需要同步调用 Rust 的 `move_highlight`
- 或者：去掉 C++ 侧的翻页逻辑，所有导航都由 Rust 侧管理

## 5. Viterbi 产生无意义组合（`diyige → 的一个`）

**现象**: `diyige` 的 Viterbi 分解 `di + yi + ge` 产生 `的一个` 这种无意义短语，排在第一。

**根因**: inputx 的 bigram 模型对短词组合缺乏语义约束。`的`(di) + `一`(yi) + `个`(ge) 在 bigram 概率上可能很高，但实际不是一个有意义的词。

**方向**:
- LargeDict 应覆盖这类常见输入，用 0.95 分压制 Viterbi 的 0.95 分
- 对 Viterbi 结果做最小语义过滤：至少 1 个实词（非虚词），或者长度 >= 3 字
- 增加虚词黑名单：`的/了/是/在/和/个/有/不/这/也/就/都/还/要/会/能/可/...`
- 如果候选词完全由虚词组成，降低其分数

## 6. 输入一半时, 鼠标触发切换输入框

此时,一个输入框中的输入内容会残留到另一个输入框中;

## 
jiushi -> 九十 

用户在选择了 就是 之后, 就是仅仅提升至第二位, 预期是第一位;

## 7. 候选权重计算 — fcitx5 参考分析

### fcitx5 + libime 架构

```
用户输入 Pinyin
  │
  ├─ PinyinDecoder::decode()
  │     ├─ 构建 Latice (所有可行音节分词)
  │     ├─ 系统 Bigram Model (sc.dict, 2.6MB)
  │     ├─ UserLanguageModel (HistoryBigram, 用户历史加权)
  │     └─ Viterbi 最优路径 → candidatesToCursor[]
  │
  ├─ 每轮 commit → context_.learn()
  │     └─ model()->history().add(words, codes) → 更新用户 Bigram
  │
  ├─ 自定义短语 CustomPhraseDict
  │     ├─ addPhrase(key, value, order)
  │     ├─ pinPhrase(key, value) → order = 0 (最高优先级)
  │     └─ Trie 前缀查找 → 插入候选列表
  │
  └─ 持久化
        ├─ model()->save(out) / model()->load(in)
        └─ customPhrase_.save(out) / customPhrase_.load(in)
```

### 权重分层（参考 `pinyin.cpp:757-790`）

| 层 | 数据源 | 评分方式 | 持久化 |
|----|--------|---------|--------|
| L1 系统词典 | `sc.dict` (2.6MB 二进制 FST) | 内嵌 unigram 频率 | 只读 |
| L2 系统 Bigram | `libime::LanguageModel` | 相邻词转移概率 | 只读 |
| L3 用户 Bigram | `UserLanguageModel` → `HistoryBigram` | `learn()` 每次 commit 更新 | `save()`/`load()` 到 `~/.local/share/fcitx5/pinyin/user.history` |
| L4 自定义短语 | `CustomPhraseDict` (Trie) | `order` 字段 (0=最高优先) | `save()`/`load()` |

### swift-ime 对照

| fcitx5 层 | swift-ime 当前 | 差距 |
|-----------|---------------|------|
| L1 unigram | inputx L1 FST (3.9MB) + LargeDict (rime-ice 900K) | 接近，但缺少 `sc.dict` 级别的权威词频 |
| L2 Bigram | inputx `top_k_compositions` (Viterbi + bigram) | 功能等同 ✅ |
| L3 用户历史 | inputx L0 `record_pick` + PinyinFamily `learn_phrase` | `record_pick` 仅对 L1 词有效，rime-ice 词走 PhraseBook。缺 HistoryBigram 型的大数据量用户模型 |
| L4 自定义短语 | PhraseBook (HashMap 前缀匹配) | 缺 `order` 优先级字段，缺 Trie 高效前缀查找 |

### 推荐优化路线

1. **统一 L1 词典**：将 rime-ice 数据**注入 inputx FST**（使用 `inputx_fsa::DictBuilder`），而非单独的 LargeDict HashMap。这样 inputx 的 L0/L1 机制自然覆盖全部词汇。

2. **升级用户模型**：从 per-pinyin L0 pin → **per-word-bigram 计数**。每次 commit 记录 `(prev_word, next_word)` 对频率，查询时对匹配的 bigram 加权。

   ```
   // 当前: pin 整个 pinyin → 一个词
   dict.pin("jishi", "即使")

   // 目标: 用户 bigram → 上下文相关
   user_bigram.add("的", "即使")  // 上次 commit 后已知 "的"+"即使" 被使用
   → 后续输入 "jishi" 且上文为 "的" 时，额外加权 "即使"
   ```

3. **自定义短语支持 `order`**：PhraseBook 增加 priority 字段，用户可手动调整。

4. **虚词过滤**：✅ 已实现（`all_stopwords()` ×0.5 惩罚）

## 行动计划

| 优先级 | 问题 | 负责 |
|--------|------|------|
| P0 | #5 — `diyige → 的一个` | ✅ 已修复（虚词 ×0.5） |
| P0 | #4 — 翻页空格错误 | C++ / Rust 同步 |
| P1 | #2 — 造词权重 | L0 pin + PhraseBook 双写 |
| P1 | #1 — 候选数量 | 新词典源 |
| P1 | #6 — 统一 L1 词典 | inputx FST 编译 |
| P2 | #6 — 用户 Bigram 模型 | HistoryBigram-style |
| P2 | #3 — 惰性加载 | 新 API |
