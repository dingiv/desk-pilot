# round16 — 统一记忆模块层(MemoryLayer / overlay dict)

需求:把 recency(最近使用加权)、单词本(自生词)、上下文感知 三个模块
用**统一的记忆模块层**重构:overlay dict 记录「提交的最终词 → (提交时
拼音, 提交时间, 累计次数)」,在后处理流水线落笔,并反向影响预测
(上下文 / 预测来源 / 权重微调:增强与消减)。

## 一、新核心:`store/memory.rs` — MemoryLayer

```rust
pub struct MemoryLayer { entries: HashMap<String, MemEntry> }  // overlay dict
pub struct MemEntry { pinyin: String, last_ms: i64, count: u32 }
```

API 与语义:

| 方法 | 语义 |
|---|---|
| `record_commit(word, pinyin, now)` | 后处理提交登记:记录映射对、计数 +1、时间盖章 |
| `register_self_generated(word, pinyin)` | 自生词入册(count = 0,提交后增长) |
| `tier(word, now) -> 0..5` | 近期指数(时间 5 档 + **频次增强**:≥3 次提交档位 +1;>3 天惰性移出 = 消减) |
| `lookup_pinyin(prefix)` | overlay 作为预测源的查询接口(phase-2 家族直读) |
| `dump / load_bulk` | 持久化(含旧 recency 表迁移种子,count=0) |

**所有权**:随 `WordBook` 归持久化模块(round14 同构);`Arc<WordBook>`
共享下用 `Mutex<MemoryLayer>` 内部可变。家族、后处理、warm 全走
`wordbook.memory` 一份。

## 二、三条影响通道的落位

1. **整合上下文感知**:拼音 `record_pick`(选词即提交的中央点)与英文
   `record_commit` 现在都写 MemoryLayer —— 最近输入记录集中化,下一次
   预测时拼音 Layer 1 / 英文 `apply_recency` 读 overlay 加成
   (z = (1-a)(a+b)/8 + a 公式不变,`b` 现在含频次增强)。
2. **自生词入 overlay**:`learn_composed_phrase`(拼音组合造词)、
   `record_learned_word` / `learn_word`(英文自生词)同步登记 overlay
   (带拼音映射对);`lookup_pinyin` 已就绪,phase-2 家族直读 overlay
   作为预测来源。
3. **权重微调(增强 + 消减)**:多次提交(count ≥ 3)→ 档位 +1;
   最近提交 → 高档位;超 3 天 → 条目移出、加成归零。微调施加点与旧
   recency 完全一致(家族预测收尾),排序行为兼容。

## 三、持久化与迁移

- 新表 `memory(word, pinyin, last_ms, count)`;`save_memory` 全量快照
  双写(拼音侧提交路径,与旧 recency 同拍),英文侧进程内(旧行为一致)。
- 旧 `recency` 表:启动时作为迁移种子灌入 overlay(count=0,
  `load_bulk` 跳过已有键);`recency.rs`(RecentStore)整文件退役,
  两份 recency 从此归一。

## 四、phase-2(后续轮次)

- 拼音 `PhraseBook` / 英文 `user_words` 的预测源直读 overlay
  (涉及 40+ 引用,单轮风险大,本轮只做登记镜像);
- 上下文 bigram(`last_commit` → `InputContext`)迁到 memory 统一出数。

## 五、验证

- MemoryLayer 7 个单元测试(映射对 / 时间分档 / 频次增强 / 惰性淘汰 /
  自生词零计数 / 前缀查询 / 持久化 + 迁移);集成测试改读 memory 表;
- 全套 211 passed;clippy 双 crate 0;评测持平 98.0 / 99.4。
