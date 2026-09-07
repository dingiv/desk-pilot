# Overlay 用户词典体系(三级架构)

> **状态:三级架构已实施(round16–19,见 `changelog.md`);Phase 0 词典
> 清洗仍为待办。** 本文合并原 `overlay.md`(早期设计稿)、
> `overlay-dict-arch.md`(round16–18 两级实现)、`overlay-three-tier.md`
> (round19 三级设计),为唯一的 overlay 体系文档。代码为准:
> `crates/ime-core/src/store/{memory,overlay_dict,wordbook}.rs`、
> `family/pinyin/merged.rs`、`family/pinyin/lattice.rs`。

## 一、动机

Seed 词典(rime-ice.fst)是"死"的、非用户定制的。引擎此前记录的是
"语料库认为什么常用",不是"这个人常用什么"。Overlay 体系提供**单用户
专一优化**:用户反复输入的词在后续预测中增强;系统词典没有的自生词
也进入使用统计并直接出候选。

## 二、三级架构(已实施)

```text
L1  OverlayData(内存,实时)         小而热:当前工作集
    · store/memory.rs — MemoryLayer,freq / recent 双表(频率与时间
      统计分表)
    · 每个新词进来 → 全套重建流程(重算有效频率、重排);量小成本可忽略
    · 达到阈值(128)→ 整体持久化到 L2,然后清空
    · 上限 512 条,超限淘汰最旧

L2  OverlayDict(持久化,冷加载)      大而稳:历史沉淀
    · store/overlay_dict.rs;SQLite overlay_freq / overlay_recent 两表
    · 启动冷加载,独立成层(不与 SeedDict 合并)
    · 直接作为预测候选来源之一(自生词即使不在 seed 也能出候选)
    · 会话内只读(被 L1 覆盖时不改写;下一次 flush 才吸收新状态)

L3  SeedDict(不可变)                rime-ice.fst + 用户静态词典
```

查找优先级(同词多层命中时,**越热越权威**):

```text
L1 OverlayData  >  L2 OverlayDict  >  L3 SeedDict
   (实时增强)      (上次沉淀)         (原始频率)
```

### 2.1 MemoryLayer(L1)

一条记忆 = 提交映射对 + 统计 + 词典域频率:

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
> 权重浮点。两者经 `LatticeDecoder::freq_to_score`(log₂ 归一)相连:
> frequency → 运行时权重。**最终决定预测顺序的是运行时权重**;
> frequency 只是它的词典域来源。

**频次增强(round18)**:有效频率在基础频率上线性增强,读取时派生,
存储只存基础值(无写放大):

```text
effective_frequency = frequency + min(count × 10, 5_000)
                      // FREQ_ENHANCE_STEP=10, FREQ_ENHANCE_CAP=5_000
```

- 自生词(基础 `SELF_GEN_FREQUENCY = 100`)随使用稳步爬升;
- 常用种子词(频次数千)相对漂移可忽略 → 排序近似稳定(eval 持平)。

关键 API:

| 方法 | 语义 |
|---|---|
| `record_commit_weighted(word, pinyin, now, weight)` | 提交登记:映射对 + 计数 +1 + 继承/设定权威权重 |
| `register_self_generated(word, pinyin)` | 自生词入册(count=0,基础频率 100) |
| `tier(word, now) -> 0..5` | 近期指数:时间 5 档 + 频次增强(≥3 次提交档位+1,封顶 5);>3 天移出 |
| `effective_frequency()` | 基础频率 + count 增强(读取时派生) |
| `flush_into(&OverlayDict)` | 达阈值整体搬入 L2 |
| `load_legacy / load_legacy_recent` | 旧 memory 单表 / 旧 recency 表迁移 |

### 2.2 OverlayDict(L2)

- `absorb(FlushBatch)`:同词 count 累加、频率取 `max(L2 基础, L1 有效)`、
  时间取新、generation 递增;
- `lookup(pinyin)`:全拼精确命中 → (词, 基础频率)——独立预测层;
- `tier(word, now)`:穿透查询(L1 时间表 miss 时继续算档,刚沉淀的词
  不丢近期加成);
- `generation`(AtomicU64):absorb / load 递增,lattice 旁路词典靠它
  比对同步(见 §四)。

### 2.3 flush(L1 → L2)

```text
L1.len() ≥ FLUSH_THRESHOLD(128)
  → 逐条 upsert 进 L2 → L1 清空 → L2 全量快照落盘(双表同步搬运)
```

进入 L2 的条目以"基础频率 + 累计 count"形态沉淀;有效频率读取时派生。
**关闭保底**:引擎 `Drop` 时 L1 无条件 flush——未达阈值的会话数据不丢
(CLI 单词会话靠它跨进程存活)。周期化的时间策略待设计(见 §六)。

## 三、权重从何而来(继承链,round19)

提交路径按词条来源三级回溯(`PinyinFamily::record_pick`):

```text
L2 命中        → 继承 L2 基础频率
L3 命中        → 查 lattice(FST)words_for(pinyin) 继承原始频率
否则(自生词)  → SELF_GEN_FREQUENCY(100),并同步写 Wordbook
```

→ OverlayData = 种子词典权重继承 ∪ Wordbook 自生词。用户的反复使用
不改变权威权重,频次影响走统计微调(tier / 频次增强)。

## 四、预测路径(读取)

### 4.1 MergedDict 三级覆盖 + 独立出候选

`family/pinyin/merged.rs::apply_overlay_override`,在 `predict_inner`
收尾(所有 bonus 之后、排序之前)施加:

```text
候选来源:L3(现有 FST/lattice 路径,不变)
        + L2(pinyin == input 的词条直接出候选,freq_to_score 同刻度)
        + L1(工作集词条同上;量小,线性扫)
覆盖:同词多层命中 → 高优先层的有效频率换算运行时权重
      独立词条 source = "overlay" 注入(上限 8;自生词主路)
微调:tier(recency + 频次增强)在覆盖之后照旧叠加(不变)
```

**排序兼容性(设计核心)**:种子词继承的 frequency == FST 原频率,同刻度
线性重标 → 分数与纯种子路径完全一致 → 覆盖不改变排序。eval 持平
(98.0 / 99.4)即这一性质的直接验证。

### 4.2 自生词进 lattice(round19 续)

`LatticeDecoder` 挂 **overlay 旁路词典**(生成号驱动):

- `overlay: RwLock<OverlayMap>`(拼音 → (词, 基础频率, count))+
  `overlay_initials: RwLock<OverlayInitialsIndex>`(声母索引,Mixed/
  Initials 对齐用);`set_overlay_entries` 整体替换;
- 五个查询点全部与 FST 合并:`words_for`(单字区/继承/整词联想)、
  predict exact、predict Mixed/Initials 声母对齐、predict_prefix;
  同词双册时 overlay 有效频率胜出;
- 同步:L2 generation 比对缓存代,变了才重灌(避免每查询重灌);
  predict_inner / predict_chained 入口各调一次,warm 后主动同步一次。

### 4.3 三层词典同一查询方式(用户指令)

对同一预测,三层词典用**同一方式**查询:全拼精确命中 → (词, 有效频率)。
三层完全一致,只是优先级不同。

## 五、数据流(一次提交的生命周期)

```text
用户选词/提交
  → stage2 产出 CommitReceipt
  → ControlPane 统一结算 post::learn_commit(receipt, wordbook, env)
      ├─ PinyinPick → record_pick:L0 计数 + last_commit(bigram 上下文)
      │               + memory.record_commit_weighted(三级继承频率)
      ├─ Commit     → wordbook.record_commit(按家族分流)+ commit_len
      ├─ ComposedPhrase → learn_composed_phrase(Wordbook)
      │               + memory.register_self_generated(基础 100)
      └─ Ascii      → english learn_word(Wordbook)+ register_memory
  → L1 满 128 条(或引擎 Drop)→ flush 进 L2 → 双表落盘
  → 下一次预测:三级覆盖 + lattice 旁路 + tier 微调 + 上下文感知
```

启动:`SeedDict 加载 → L2 冷加载(overlay_freq/overlay_recent)→ L1 空`;
`warm_memory` 做旧表(memory 单表 / recency 表)一次性迁移。

**所有权链**:PersistenceManager(所有者)→ `Arc<WordBook>` →
`{ pinyin: PinyinBook, english: EnglishBook, memory: Mutex<MemoryLayer>,
overlay_dict: Mutex<OverlayDict> }`,Arc 分发至 SessionState / 两家族 /
post::learn_commit。

## 六、待办与演进

| 项 | 现状 | 目标 |
|---|---|---|
| OverlayDict 周期化 | 阈值 128 触发 + Drop 保底 | 定时/定量的时间策略(待设计) |
| Phase 0 词典清洗 | 未做(见 §七) | 清洗后继承权重才完全保真 |
| 家族预测源直读 overlay | 已部分完成(lattice 旁路);PhraseBook/en_user 仍是自生词预测源 | 40+ 引用迁移(单轮风险大) |
| bigram 上下文迁 memory | `last_commit` 仍在拼音家族内 | 上下文统一从 MemoryLayer 出数 |
| 英文 overlay 覆盖 | 英文仅 tier 微调 | 对齐拼音侧三级覆盖语义 |

## 七、Phase 0 — 词典大清洗(前置待办,设计保留)

rime-ice(91.6 万条 → 22MB FST)的 weight **两种量纲混用**:单字是真实
语料字频(的 76,938,354),多字词组是词库作者手工标注等级("版权"
13,204,281、人工抬顶词条 19,260,817)。后果:量纲断崖 58 倍、同档同分
(及时/即使、出示/初始)、人名/诗性词/拟声词占池、敏感词条随库带入。

清洗方案(设计于早期,实施待定):

- 数据源:OpenSubtitles 中文词频(hermitdave/FrequencyWords,
  `zh_50k.txt`,与英文侧 `hermitdave.tsv` 同源同格式,单字与词组同一
  量纲);
- 管线:`refine_dict.sh`(rime-ice TSV + zh_freq.tsv → 清洗规则 →
  重建 FST)。规则:语料覆盖词条改 weight = 语料 count;未覆盖降置信
  (×0.05);拟声/重复词 ×0.1;人名/诗性词删除;blocklist 人工审阅;
- 验收:`jishi → 及时 #1`、`chushi → 出示 #1`;tc_dict_sample ≥ 98%;
  golden 断言逐条复核(权重域整体移动,修订须以清洗报告佐证)。
