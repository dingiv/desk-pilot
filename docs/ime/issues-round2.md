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

Top-1 未命中的最后 2 条。可能需要：
- 用户 Bigram 上下文加权（见 #3）
- rime-ice 权重微调

### 2. 候选词数量 + 惰性加载

**现象**: 所有家族一次性生成全部候选词，被 `take(N)` 截断。
**方向**:
- `CandidateFamily` 新增 `predict_more(input, offset, limit)`
- C ABI 新增 `swift_ime_predict_more(ctx, page)` 接口
- 懒加载翻页

### 3. 用户 Bigram 模型升级

**现状**: `UserBigram` 已实现内存中的 (prev_word, next_word) 计数 + `bigram_boost()`。
**差距**: 
- 未接入 fcitx5 的 commit 回调（每次上屏应调用 `record_bigram`）
- 持久化走 SQLite `weight_store`，但 fcitx5 C++ 侧未调用保存

### 4. L1 FST 词典注入

将 rime-ice 数据注入 inputx FST（使用 `DictBuilder`），使 inputx 的 L0/L1 机制自然覆盖全部词汇，而非分开维护 LargeDict + inputx dict。

### 5. 输入框切换时状态残留

一个输入框的 preedit 残留到另一个输入框。需要 C++ 侧在 focus_out 时调 `swift_ime_reset()`。

## 📋 行动计划

| 优先级 | 问题 | 状态 |
|--------|------|------|
| P0 | 权重评分 — `diyige → 的一个` | ✅ 已修复 |
| P0 | 翻页空格错误 | ✅ 已修复 |
| P0 | 权重无区分度 | ✅ 已修复（Top-1 87.5%） |
| P0 | dict/viterbi/jianpin 三套独立 | ✅ 已统一为 LatticeDecoder |
| P1 | `jishi→即使` #2 | 🔧 待优化 |
| P1 | `chushi→初始` #2 | 🔧 待优化 |
| P1 | 造词权重 | 🔧 部分完成 |
| P1 | 候选数量 | 🔧 待实现 |
| P1 | 统一 L1 词典 | 🔧 待实现 |
| P2 | 用户 Bigram 模型 | 🔧 部分完成 |
| P2 | 惰性加载 | 🔧 待实现 |
| P2 | 输入框切换状态残留 | 🔧 待修复 |
