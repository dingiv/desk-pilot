# Overlay Dict 体系架构 — 多词典与统一记忆层

> **状态:已实施(round14–17),本文为实现文档。**
> 关联:`overlay.md`(早期设计稿,Phase 0 词典清洗仍为前置议题)、
> `issues-round14.md`(WordBook 抽取)、`issues-round16.md`(MemoryLayer)、
> `issues-round17.md`(多词典体系)。

## 一、要解决的问题

用户模型此前散落在三处且互不知情:

| 旧模块 | 数据 | 缺口 |
|---|---|---|
| recency(拼音/英文各一份 `RecentStore`) | word → last_ms | 纯时间衰减,无频次、无权重、双份冗余 |
| 单词本(PhraseBook / en_user) | 自生词 → count/score | 与 recency 无关联,不承载提交统计 |
| 上下文感知(`last_commit` / `InputContext`) | 最近提交文本 | 提交记录分散,权重与上下文两张皮 |

round16/17 以**一个统一记忆层**收拢:所有"用户提交了什么、什么时候、
多频繁、带什么权重"集中到一个 overlay 数据层,再以**多词典体系**反哺预测。

## 二、体系全景(磁盘 / 内存)

```text
┌─ 磁盘 ────────────────────────────────────────────────────────┐
│ SeedDict     apps/swift-ime/assets/dict/rime/rime-ice.fst     │
│              key = 拼音\x00词,value = 词频(22MB,静态)      │
│ OverlayDict  SQLite memory 表(word, pinyin, last_ms,        │
│              count, weight)—— OverlayData 的规范化快照;      │
│              周期性生成的时间策略待设计(当前:提交即双写)     │
│ Wordbook     WordBook 相关表(phrases / en_user)—— 运行时     │
│              实时同步的自生词:词 + 拼音 + 词频               │
├─ 内存 ────────────────────────────────────────────────────────┤
│ SeedDict / OverlayDict / Wordbook —— 同上,启动加载          │
│ OverlayData  MemoryLayer(store/memory.rs)—— 实时动态       │
│              overlay,后处理直接读写                          │
│ MergedDict   SeedDict × OverlayData 合并视图                  │
│              (family/pinyin/merged.rs,查询时合并)            │
└───────────────────────────────────────────────────────────────┘
```

**所有权链**(round14 确立,round16 延续):

```text
PersistenceManager(所有者)
   └─ Arc<WordBook> ──┬─ PinyinBook { phrase_book, store }
                      ├─ EnglishBook { user_words, store }
                      └─ memory: Mutex<MemoryLayer>   ← OverlayData
                           Arc 分发 → SessionState / PinyinFamily /
                           EnglishFamily / post::learn_commit
```

## 三、MemoryLayer(OverlayData)

`store/memory.rs`。一条记忆 = 提交映射对 + 统计 + overlay 权重:

```rust
pub struct MemEntry {
    pub pinyin: String,    // 提交时拼音(映射对另一半;英文为空)
    pub last_ms: i64,      // 最近提交时间(wall-clock ms)
    pub count: u32,        // 累计提交次数(自生词登记 = 0)
    pub frequency: u64,    // 词典域基础频率(0 = 尚无,如旧迁移种子)
}
```

> **量纲申明(round18)**:`frequency` 是**词典域频率**(与 FST value
> 同量纲,如 rime-ice 的成千上万级词频),**不是**程序运行时 0..1 的
> 权重浮点。两者经 `LatticeDecoder::freq_to_score` 线性重标(log₂ 归一)
> 相连:frequency → 运行时权重。**最终决定预测顺序的,是运行时权重值**;
> frequency 只是它的词典域来源。

### 频次增强(round18)

随提交次数增长,**有效频率**在基础频率上线性增强(读取时派生,存储
存基础值 —— 稳定、无写放大):

```rust
effective_frequency = frequency + min(count × FREQ_ENHANCE_STEP, FREQ_ENHANCE_CAP)
                      // STEP = 10,CAP = 5_000(中等词频量级封顶)
```

- 自生词(基础 100)随使用稳步爬升 → 在 MergedDict 覆盖中排名上移;
- 常用种子词(频次数千)相对漂移可忽略 → 排序近似稳定(eval 持平);
- MergedDict 覆盖算法改用 `effective_frequency()` 换算运行时权重。

API:

| 方法 | 语义 |
|---|---|
| `record_commit(word, pinyin, now)` | 提交登记:映射对 + 计数 +1 + 盖章 |
| `record_commit_weighted(…, weight)` | 同上,并可设定/继承权威权重 |
| `register_self_generated(word, pinyin)` | 自生词入册(count=0,weight=默认 100) |
| `tier(word, now) -> 0..5` | 近期指数:时间 5 档 + **频次增强**(≥3 次提交档位+1,封顶 5);>3 天惰性移出(消减) |
| `entry(word) / lookup_pinyin(prefix)` | 查询(诊断 / 预测源接口) |
| `dump / load_bulk` | 持久化(5 元组,**基础频率**;增强读取时派生;旧 recency 表迁移种子) |

容量:512 条上限,超限淘汰最旧。写穿:拼音侧提交路径全量快照
`save_memory`(≤512 行单事务),英文侧进程内(与旧 recency 行为一致)。

## 四、OverlayData 权重从何而来(继承规则)

提交路径按词条来源二分:

1. **词在 SeedDict**(`PinyinFamily::record_pick`,选词即提交的中央点):
   查 lattice(FST)`words_for(pinyin)` 取**原始频率分继承**为 overlay
   weight —— 用户的反复使用不改变权威权重,频次影响走统计微调(tier)。
2. **自生词**(learn 路径:`learn_composed_phrase` / `record_learned_word`):
   固定默认 `SELF_GEN_WEIGHT = 100`(与 build_dict 对无权重 TSV 的缺省
   一致),同时同步记入 Wordbook。

→ OverlayData = SeedDict 权重继承 ∪ Wordbook 自生词,记录用户最近
输入的词汇及其权威权重。

## 五、MergedDict 与权重覆盖算法

`family/pinyin/merged.rs::apply_overlay_override`。拼音预测同时使用
MergedDict(种子词典查询路径)与 OverlayData;**同一预测选项**(同词
且拼音映射对 == 当前输入)在两边都出现时,**以 overlay 权重为准**:

```text
候选命中 overlay(weight > 0 且 pinyin == input)
    → raw_score = freq_to_score(weight)   // 与 FST 同一刻度
```

施加点:`predict_inner` 收尾(所有 bonus 之后、排序之前)。

**排序兼容性(设计核心)**:种子词继承的 frequency == FST 原频率,同刻度
线性重标(`LatticeDecoder::freq_to_score`,log₂ 归一化到
[min_score, max_score])→ 分数与纯种子路径完全一致 → 覆盖不改变排序。
自生词(phrase 来源)则被规范进统一词频域。实测 eval 持平
(98.0 / 99.4)是这一性质直接验证。

**两层语义不混**:overlay 定**基线**(有效频率),统计做**微调**
(recency tier / 频次增强,在覆盖之后照旧叠加)。即:
`score = freq_to_score(overlay.weight)` 之后,再吃
`z = (1-a)(a+b)/8 + a` 的近期加成。

## 六、数据流(一次提交的生命周期)

```text
用户选词/提交
  → stage2 产出 CommitReceipt(round14)
  → ControlPane 统一结算 post::learn_commit(receipt, wordbook, env)
      ├─ PinyinPick → record_pick:L0 计数 + last_commit(bigram 上下文)
      │               + memory.record_commit_weighted(继承种子频率)
      ├─ Commit     → wordbook.record_commit(按家族分流)+ commit_len
      ├─ ComposedPhrase → learn_composed_phrase(Wordbook)
      │               + memory.register_self_generated(默认权重)
      └─ Ascii      → english learn_word(Wordbook)+ register_memory
  → 拼音侧快照双写 OverlayDict(SQLite memory 表)
  → 下一次预测:MergedDict 覆盖 + tier 微调 + 上下文感知
```

启动:`warm_all` → `load_memory` 灌 OverlayData;旧 `recency` 表作迁移
种子(count=0, weight=0,自然枯竭);`load_bulk` 丢弃 >3 天条目。

## 七、演进路线(phase-2 / 待设计项)

| 项 | 现状 | 目标 |
|---|---|---|
| OverlayDict 周期化 | 提交即全量快照双写 | 定时/定量规范化生成(时间策略待设计) |
| 家族预测源直读 overlay | PhraseBook/user_words 仍是自生词预测源;overlay 仅作权重覆盖 | 拼音/英文直接 `lookup_pinyin` 取候选(40+ 引用迁移) |
| bigram 上下文迁 memory | `last_commit` 在拼音家族内 | 上下文统一从 MemoryLayer 出数 |
| Phase 0 词典清洗 | 未做(见 overlay.md) | rime-ice 两种量纲混用清洗后,继承权重才完全保真 |
| 英文 overlay 覆盖 | 英文仅 tier 微调,无权重覆盖 | 对齐拼音侧 MergedDict 语义 |
