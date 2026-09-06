# swift-ime 第十四轮 — 学习功能出家族,归后处理

> 创建: 2026-09-06。用户模型:"拼音家族跟英文家族不应该感知 commit ——
> 它们只根据家族内部状态 + 外部传入的 context 做预测,是干净的状态机。
> 单词本和学习功能移到后处理,后处理感知候选来自哪个家族。"

## 一、现状(改造前)

stage2(`family_prediction.rs`)的提交路径直接调学习接口:

- `select` 整词提交 → `env.record_pick`(L0)+ `env.learn_composed_phrase`(自生词)
- `select` 逐字提交 → `env.record_pick`
- `commit_text` → `env.record_commit_text`(recency 分流)+ `record_commit_len`
  + `env.learn_ascii_word`(英文自生词)

家族因此**感知了 commit 动作** —— 预测状态机不干净。

## 二、改造(round14 落地)

### 2.1 学习回执(CommitReceipt,post.rs)

stage2 提交路径只产出**纯数据回执**,不再调任何学习接口:

```rust
pub(crate) enum CommitReceipt {
    PinyinPick    { pinyin, word },          // 拼音 L0 频率加成(整词/逐字)
    ComposedPhrase{ pinyin, text },          // 自生词(逐字选择后整体提交)
    Commit        { text, family },          // 提交落地(recency 分流/ASCII/长度)
}
```

### 2.2 学习结算(learn_commit,post.rs)

后处理**感知来源家族**,据此分发策略:

- `PinyinPick` → `env.record_pick`(拼音 L0)
- `ComposedPhrase` → `env.learn_composed_phrase`
- `Commit` → `record_commit_text`(english → 英文 recency,其余 → 拼音)
  + `record_commit_len`(#del 长度)
  + 纯 ASCII 字母数字且非英文族 → `learn_ascii_word`(英文自生词)

### 2.3 结算时机(ControlPane)

`ControlPane::handle_event` 在事件处理完成后 drain
`SessionState.pending_learning`,逐条交 `post::learn_commit` ——
键路径/控制路径/异步路径统一覆盖。

### 2.4 家族瘦身

- `family_prediction.rs`:删 `env.record_pick/learn_composed_phrase/
  record_commit_text/learn_ascii_word` 全部调用;`commit_text` 去掉
  `env` 参数;`commit_raw_and_reset`/`pinyin_enter`/`pinyin_terminator`
  同步去 `env`。
- 家族仍暴露 `record_*` 学习接口(单词本属于家族自身状态,是预测数据
  的一部分),但**触发权**全部上移到后处理 —— 家族不再感知 commit,
  只被后处理"投喂"。
- 上下文感知保持:`InputContext` 在预测前经 `scorer.collect` 注入各家族。

## 三、验证

- 全测试绿(149+21+7+11+14+2,含英文学习回归:
  `committing_english_dict_word_does_not_learn_it_as_user` 等);
- clippy 双 crate 0 警告;评测持平 tc_dict 98.0 / tc_en 99.4。
- fsm 层(stage2)学习接口调用数:**0**(全部经回执 → post::learn_commit)。


## 四、单词本独立模块(round14 续,同日落地)

用户模型:"单词本抽成独立模块;家族依赖单词本、后处理依赖单词本;
SessionState 持引用;所有权归持久化模块。"

### 4.1 新模块 `store/wordbook.rs`(持久化模块内)

```rust
pub struct WordBook {
    pub pinyin:  PinyinBook  { phrase_book, recency, store },  // 自生词短语本 + 最近使用
    pub english: EnglishBook { user_words,  recency, store },  // user 层 + 最近使用
}
```

学习写入(merge 语义/写穿 SQLite)随存储一起入册:
`PinyinBook::{learn_composed_phrase, record_commit, set_store}`、
`EnglishBook::{learn_word, merge_user, record_commit, set_store}`。

### 4.2 依赖方向(与规格一致)

```
PersistenceManager(所有权;init_store 时 Arc 移入)
   └─ Arc<WordBook> ──┬─ SessionState(引用;每个会话一台状态机共享)
                      ├─ PinyinFamily(预测时读册:自生词/recency 加成)
                      └─ EnglishFamily(预测时读册:user 层/recency 加成)
post::learn_commit(后处理)→ 直接写册(recency/自生词/英文自生词)
```

- 家族不再自有存储字段(phrase_book/user_words/recency/store 全部移出),
  预测代码经 `self.wordbook.pinyin/english.*` 读册。
- `post::learn_commit(receipt, wb, env)` 直接写册;env 仅剩两个例外:
  拼音 L0(inputx_pinyin 引擎词典内部,本轮不外搬,已记录为边界)与
  `record_commit_len`(魔法资源)。

### 4.3 验证

全测试绿;clippy 双 crate 0;评测持平 98.0 / 99.4。
