# swift-ime 第六轮 — 非魔法家族预测逻辑检视(pinyin + english)

> 创建: 2026-08-29。源于对 pinyin / english 两个家族预测逻辑的系统性 code review
> (emoji 仅顺带核对接口,非本轮对象)。本轮回合:只检视 + 记录,修复在后续轮次。

## 架构现状(检视时的基线)

```
query_pinyin (state.rs:1072)
  └→ UnifiedScorer::rank_detailed (family/mod.rs:233)
       ├→ PinyinFamily   (priority 100, top_n 128)
       │    单音节 exact → lattice(全拼/简拼/混写)→ 前缀联想 → Viterbi 造词
       │    → PhraseBook(exact + initials)→ 短词加分 → 排序
       ├→ EnglishFamily  (priority 70, top_n 8)
       │    user 层 → base 层(SCOWL),二分前缀扫描,exact 固定 0.88
       └→ EmojiFamily    (priority 60)
     final = raw_score × priority/100 → 全局排序 → 去重
```

## 🔴 Bug(行为级,应优先修)

### B1 · english `.en_cache` 重载损坏归一化分数

`english.rs:387` `load_cache_if_valid` 用
`parse_and_normalize(data, DictType::Grade)` 当 "passthrough"(注释原文
`Grade = Passthrough (scores already normalized)`)——**不成立**。缓存里的
分数是 decile 归一化后的任意值(1~10000),`grade_to_score(9307)` 会落进
`_ => 2000` 兜底臂。

后果:外部频率词典**首次加载正确,重启后**(缓存命中路径)全部词频分数
塌缩到 2000/5500/8000/9500 四档,前缀排序退化。exact(固定 0.88)不受影响,
所以日常难察觉。

回归测试缺口:没有"缓存重载后分数与首次加载一致"的用例。

### B2 · 双 pinyin 引擎实例脑裂

两份 `inputx_pinyin::PinyinEngine` 实例并存:

| 实例 | 位置 | 消费方 |
|---|---|---|
| A:`InputxPinyin` | `dispatcher.rs:63` `Dispatcher.pinyin` | `env.pinyin()` → state.rs:1104 造词单字候选 |
| B:`PinyinFamily.engine` | family/mod.rs 构造 | 家族 predict + `record_pick`(L0 钉选/频次)+ `in_dictionary` |

`record_pick` 只落 B(dispatcher.rs:225)。**用户的选择历史(L0)永远不影响
造词单字候选的排序**——A 的频次模型保持出厂状态。是缺陷还是刻意取舍,代码
无声明;若为取舍应注释,若为缺陷应共享实例或双写。

### B3 · 运行时家族开关对 pinyin/english 是死旋钮

`CandidateFamily::set_family_enabled`(family/mod.rs:108)默认 no-op,**只有
emoji 覆盖**(emoji.rs:244)。同时 `PinyinFamily::enabled` / `EnglishFamily::enabled`
经 `set_enabled(&mut self)` 设置,但家族装进 `Box<dyn CandidateFamily>` 后
`&mut` 不可达(emoji 走 `&self` + 内部 `Mutex<bool>`,另两家族没有)。

后果:任何通过 `set_family_enabled("pinyin"/"english", false)` 的禁用**静默
无效**;`UnifiedScorer::family_count()` 照常计数。配置写了不生效且无告警。

## 🟡 规范问题(本轮梳理对象)

### D1 · `InputContext` 是幻觉管道

- pinyin `predict_with_context` 签名收 `_ctx` 然后**忽略**(pinyin/mod.rs:718),
  自用 `last_commit`(在 `record_pick` 里另记一份,pinyin/mod.rs:413);
- english / emoji 不覆盖,delegation 到 `predict`(ctx 全丢);
- state.rs:747 认真维护 `context.update()` 并层层传入。

同一份"上次提交"状态存两处(`InputContext.last_word` 与
`PinyinFamily.last_commit`),其中 trait 管道那条**彻底无人消费**。

### D2 · 三层截断职责不清

| 层 | 位置 | 值 |
|---|---|---|
| trait `top_n` | family/mod.rs:112(默认 8) | pinyin **128**(≈不截断)/ english 8 |
| 家族内部 take | `PinyinWeights::{large_dict_take:96, viterbi_take:48, jianpin_take:8}` | 引擎效率层 |
| 家族内部 truncate | english.rs:558 `truncate(16)` | 又一道 |

同一个"最多出几个候选"概念有三处实现,调参时不知道动哪个。规范化方向:
家族内部 take 明确为**引擎预过滤**(性能语义),对外的唯一截断入口是
`UnifiedScorer` 的 `top_n`。

### D3 · 打分公式三套并存 + 同名不同义

- lattice 词频:`FreqScale` log₂ 归一(scoring.rs:70);
- english 词频:decile 1-10000 + 四档 `freq_to_score`(english.rs:410);
- pinyin 成员分:常数表(`PinyinWeights`)。

且 `freq_to_score` **两个同名不同实现**(lattice.rs:406 log 映射 vs
english.rs:410 分档),读代码极易混淆。映射规则各自合理,但缺少一张统一
的"分数从哪来"对照表(weight-scoring.md 应承接)。

### D4 · magic numbers 一半参数化一半写死

已进 weights:`PinyinWeights`(13 个)/ `EnglishWeights`(3 个)。
仍硬编码:

| 常数 | 位置 | 值 |
|---|---|---|
| english 前缀质量式 | english.rs:481 | `0.60 + 0.25 × 词频 × 匹配率` |
| 短词降权 | english.rs:55 | `SHORT_WORD_PENALTY = 0.6` |
| phrase 分数曲线 | pinyin/mod.rs:350 | `0.70 + 0.02×(count−1)`,cap phrase_book |
| 简拼折扣二次乘 | pinyin/mod.rs:696 | initials 命中 `phrase_score × 0.95` |
| 字典大小阈值 | pinyin/mod.rs:507, 534 | `100_000` 字节 ×2 处 |
| 链式 beam | pinyin/mod.rs:222 | `K=8 / BEAM=16`(与 take 家族不一致) |
| english 内部截断 | english.rs:558 | `truncate(16)` |

同类参数两种待遇,调参体验割裂。

### D5 · `CandidateFamily` trait 肥胖,接口隔离失效

20+ 方法,8 个 default no-op。按消费方分:

- 仅 pinyin 用:`record_commit` / `warm_recencies` / `set_context_aware`
- 仅 english 用:`record_learned_word` / `warm_learned_words`
- 仅 emoji 用:`set_family_enabled`

新家族实现 trait 要面对一堆与自己无关的钩子;no-op 默认让"谁真的用了"
无法从类型上看出。

### D6 · 中英能力不对称未声明

english 无 recency(刚打过的词无提升)/ 无 context / 无短语学习;
pinyin 无英文侧学习(英文自生词走 english 自己的 `record_learned_word`)。
哪些是刻意取舍(英文频次已够准?)哪些是缺口(打字游戏术语应该有
recency?),没有任何文档裁决。

## 🟢 Nits

| 项 | 位置 |
|---|---|
| `predict` 尾部排序死分支(`if !out.is_empty() {sort;return} sort;out` 两分支等价) | pinyin/mod.rs:709-715 |
| pinyin warm 用 `eprintln!` ×2,english 用 `tracing::` —— 日志口径不一 | pinyin/mod.rs:179,485 |
| `query_layer` 9 参数(已有 allow)+ 二分比较器**永不返回 Equal** 的微妙性无注释 | english.rs:424-445 |
| 构造器 5 条路(`new/with_weights/with_scoring/with_phrase_book/with_scoring_and_phrase_book`),两条体重复制 | pinyin/mod.rs:94-157 |
| 多处 `partial_cmp().unwrap()`(当前算术不产 NaN,但模式脆弱) | pinyin/mod.rs:710 等 |
| `load_cache_if_valid` 哈希用 `DefaultHasher`(构建内确定,跨编译器版本不保证)→ 缓存无谓失效重建 | english.rs:347 |

## 规范化方向(待拍板后立项)

1. **修行为 bug**:B1(加"缓存重载分数不变"回归测试)、B3(开关补齐或删接口)、
   B2(拍板:共享引擎 or 双写 or 声明取舍)
2. **候选生成契约**:family = `(input, config) → 有序候选`;对外截断唯一入口
   `UnifiedScorer::top_n`;家族 take 更名 prefilter(性能语义)
3. **InputContext 二选一**:真用(english 接 last_word 联想)或删除;
   `last_commit` 并入唯一上下文源
4. **权重大扫除**:D4 表格里的常数全部进 `*Weights`(yaml 一处可调)
5. **trait 瘦身**:可选能力拆小 trait,或从 trait 下放到具体家族的固有方法
6. **Nits 顺手清**:死分支 / eprintln / 构造器合并

建议顺序:1(小而独立,行为正确性)→ 2+4(动面大,一次做透)→ 3/5/6。
B2 和 D1/D6 需要先拍板取舍再动代码。

## 关联

- 上一轮:issues-round5.md(黄金测试已锁中英 emoji 跨家族顺序——本轮
  规范化动权重时,它是安全网)
- weight-scoring.md(层次表;D3 的"分数来源对照表"应落入此处)
