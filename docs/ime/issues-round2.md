# swift-ime 第二轮系统优化 — 问题清单

> 最后更新: 2026-07-31

## ✅ 已解决

### 权重评分系统 — Top-1 87.5%, Top-3 100%

**根因**: `fetch_dict.sh` 砍掉了 rime-ice 的 weight 列，所有词进入 FST 时权重相同。
**修复**: 
- `fetch_dict.sh` 保留 3 列输出（pinyin, word, weight）
- `build_dict.rs` 读取第 3 列 weight → `DictBuilder.insert(pinyin, word, weight)`
- `LatticeDecoder::freq_to_score()` log₂ 归一化 [0.25, 0.90]
- 虚词黑名单 + `stopword_penalty` 0.5

### 统一 Lattice 引擎

将 `dict` + `viterbi` + `jianpin` 三个独立 member 合并为 `LatticeDecoder`：
- **全拼**: `FST.get()` O(1)
- **简拼/混写**: 声母边界 greedy_parse → initials_index 快查 → pattern_match 校验
- 启动: .fst.jianpin 缓存 ~50ms

### `diyige → 的一个` 修复

虚词黑名单（的/了/是/在/和/个/有/不/这/也/就/都/还/要/会/能/可/一/那/它/他/她/我/你/们/上/下）+ LatticeDecoder 中检查是否真实字典词，字典词跳过惩罚。

### 翻页空格错误

C++ keyEvent 拦截 Space：候选列表可见时选当前页第一候选，不传递给文档。

## 🔧 进行中 / 待优化

### 1. `jishi → 即使` 仍 #2, `chushi → 初始` 仍 #2

**已忽略。** 不再作为优化目标。Bigram 上下文加权已就绪，实际使用中用户选择会自然提升排名。

### 2. 候选词数量 + 惰性加载

**现象**: 所有家族一次性生成全部候选词，被 `take(N)` 截断。
**方向**:
- `CandidateFamily` 新增 `predict_more(input, offset, limit)`
- C ABI 新增 `swift_ime_predict_more(ctx, page)` 接口
- 懒加载翻页

### 3. 用户 Bigram 模型升级

**现状**: ✅ 已闭合持久化回路。
- 每次 commit 时，`ImeEngine::record_bigram(prev, next)` **双写** SQLite + 内存 `UserBigram`
- 启动时 `init_store()` 从 SQLite 加载全部 bigram → 内存，实现跨会话记忆
- `predict_with_context` 实时查询内存 bigram 进行上下文加权

**数据流**:
```
commit "大陆" (prev="大", next="陆")
  → record_bigram("大", "陆")
    → WeightStore::record_bigram() → SQLite (持久)
    → PinyinFamily::record_bigram() → UserBigram (内存, 即时生效)
    
下次启动:
  → init_store()
    → WeightStore::load_all_bigrams() → Vec<(prev, next, count)>
    → PinyinFamily::warm_bigrams() → UserBigram::load_bulk()
    → predict_with_context 立即可用
```

### 4. L1 FST 词典注入

将 rime-ice 数据注入 inputx FST（使用 `DictBuilder`），使 inputx 的 L0/L1 机制自然覆盖全部词汇，而非分开维护 LargeDict + inputx dict。

### 5. 输入框切换时状态残留

**已修复**: `reset()` 和 `deactivate()` 回调现在在清理 Rust 状态后，显式清除 fcitx5 InputPanel UI（`inputPanel().reset()` + `updatePreedit()` + `updateUserInterface()`）+ 擦除 `lastViews_` 防止 diff 残留。`activate()` 也加了 `lastViews_.erase()` 安全网。

## 📋 行动计划

| 优先级 | 问题 | 状态 |
|--------|------|------|
| P0 | 权重评分 — `diyige → 的一个` | ✅ 已修复 |
| P0 | 翻页空格错误 | ✅ 已修复 |
| P0 | 权重无区分度 | ✅ 已修复（Top-1 87.5%） |
| P0 | dict/viterbi/jianpin 三套独立 | ✅ 已统一为 LatticeDecoder |
| P1 | `jishi→即使` #2 | ❌ 已忽略 |
| P1 | `chushi→初始` #2 | ❌ 已忽略 |
| P1 | 造词权重 | ✅ 已闭合持久化回路（PhraseBook → SQLite → warm 跨会话） |
| P1 | 候选数量 | 🔧 待实现 |
| P1 | 统一 L1 词典 | 🔧 待实现 |
| P2 | 用户 Bigram 模型 | ✅ 已闭合持久化回路（双写 SQLite + 内存，跨会话 warm） |
| P2 | 惰性加载 | → 延后至后续轮次 |
| P2 | 输入框切换状态残留 | ✅ 已修复（reset/deactivate/activate 均清除 UI + lastViews） |

> **第二轮收尾。** 新增任务见 [第三轮 →](issues-round3.md)
