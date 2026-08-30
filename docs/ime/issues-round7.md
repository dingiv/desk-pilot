# swift-ime 第七轮 — 中英预测体验强化

> 创建: 2026-08-30。第六轮解决了卫生问题(死代码/权重归位/trait 瘦身),
> 本轮回到**预测质量本身**:让输入法"记得住上下文、跟得上语种、排得开造词"。
> 源于对两家族逻辑的体验向检视(区别于第六轮的工程向检视)。

## 现状管线(检视时基线)

```
拼音:单音节表(large_dict−线性衰减)→ lattice(全拼/混写/简拼 ×0.5)
     → 前缀联想(词频 ×0.75 × 0.85^超出衰减)→ Viterbi 造词(平分 0.4)
     → 单词本(phrase_base+step×次数)→ 两字加分
     → recency 合成 + 前缀整词联想(context_aware gate)
英文:单字母 self/case → user 层 → base 层(exact 固定 0.88 /
     prefix 地板 0.6 + 质量项)→ PREFILTER_TAKE
合成:raw × priority(100/70/60)→ 全局排序 → 去重
```

安全网:`global_ranking.rs` 10 项黄金断言锁跨家族顺序。

## 🔴 E1 · 跨提交 bigram 联想缺失(本轮核心)

**场景**:打完"今天"再打 `tianqi`——"天气"没有任何加权。每句从零开始,
上一句的词汇上下文完全不参与当前候选的排序。唯一的例外是 Layer 2 整词
联想,但它要求"上一词+当前输入拼成的**整词**"恰好在词典里
(如 今天+qi→今天天气),覆盖面窄。

**关键发现**:inputx-pinyin 1.4 的 `dict()` 上有三个**从未使用**的接口:

```rust
predict_next_words(&self, prev: &str, limit: usize) -> Vec<(String, u64)>
bigram_boost(&self, prev: Option<&str>, next: &str) -> f64   // ← 就是它
predict_next_words_context(...)
```

嵌入的 bigram 语料(含 INTRA)已经随 crate 进内存,而我们手里
`last_commit(word, pinyin)` 的数据管道也是通的(`record_pick` 维护)——
**只差一次接线**。这也正是第六轮 D1 保留 InputContext 管道所等的东西。

**实现草案**:
- `predict_with_context`(或独立 Layer)里,对与输入同拼音键的候选
  施加 `bigram_boost(Some(&last_commit.word), candidate)`:
  提升方式待接口返回值标定(乘法 or 加法,clamp 防顶满)
- bigram 无数据的候选不动 —— 纯增益,不伤现有排序
- `context_aware: false` 时跳过(与 recency 同 gate)

**风险**:bigram 分数与现有 raw 的量纲对齐需要实验;golden 测试护航。

## 🟡 E2 · 英文 recency(D6 能力矩阵留的口子)

**场景**:反复打 `prediction` / `rectangle` 这类标识符,每次都靠前缀
重匹配;曾经完整打过的词没有任何"刚打过"加权。

**现状**:`dispatcher.record_commit` 入口已存在,但只路由给拼音家族
——英文候选提交时 commit_family == "english",什么也不记。

**实现草案**:
- `EnglishFamily` 加 `recency: RecentStore`(结构直接复用拼音侧的)
- dispatcher 按家族分派 record_commit(替代现在的硬编码 "pinyin")
- prefix/exact 候选命中 recency 时 +boost(乘法合成,复用拼音的
  `(1-a)(a+b)/8 + a` 公式 —— 天然 <1 不顶满)

## 🟡 E3 · 输入语言自适应(需要行为设计,可下轮)

**场景**:连打 5 个英文单词后输入 `an`——候选"安"仍压过 `an`/`An`。
用户此刻大概率在写英文句子。

**现状**:`last_commit_family` 已存在(engine 记录,学习分派用),
但只影响"学不学",不影响"排不排"。

**实现草案**:最近 N(=3?)次提交全为 english → english 家族 priority
70→85 临时抬升;出现中文提交立即回落。priority 是合成乘数,抬升只影响
跨家族竞争,不动家族内排序。**决策点**:窗口大小、抬升幅度、
单字母(已有 self/case 置顶)是否豁免。

## 🟢 E4 · Viterbi 死权重激活(顺带)

`PinyinWeights.viterbi_base(0.25) / viterbi_scale(0.55)` **定义了但
零使用**——decomp 候选写死平分 0.4,"下一个"和瞎拆排不开
(靠 top_k_compositions 的返回序透传,sort 稳定才没乱)。

**实现草案**:`raw = viterbi_base + viterbi_scale × 归一化(内部分)`;
内部分来自 `top_k_compositions` 返回的 f64(被 `_s` 丢弃中)。
归一化基准待定(组内 max 或 char_max_freq)。

## 🟢 E5 · 单音节衰减曲线依赖 N(低优先)

`large_dict − (i/total) × decay`:衰减率随候选总数 N 变化——候选多的
音节(a,yi)尾部掉到 0.35,候选少的音节几乎不衰减,同一输入法内曲线
不一致。可改固定比例:`large_dict × (1 − ratio^i)` 之类。
**建议本轮不做**(现有曲线无实际投诉,动它碰 golden)。

## 🟢 E6 · english exact 接词频(谨慎)

exact 固定 0.88 不看词频:the/and 与低频词同权(同输入只有一个 exact,
家族内无排序问题;影响的是跨家族边界与 prefix 的 quality 项联动)。
动它会碰跨家族层次(0.88×0.70=0.616 vs 简拼 0.503 的余量)。
**建议本轮不做**,或做成独立 weight 实验后再定。

## 建议范围(待拍板)

| # | 项 | 规模 | 依赖 |
|---|---|---|---|
| E1 | 跨提交 bigram 联想 | 中(~100 行 + 标定实验) | 无 |
| E2 | 英文 recency | 小(~60 行) | 无 |
| E4 | Viterbi 死权重激活 | 小(~30 行) | 无 |
| E3 | 输入语言自适应 | 中 | 行为设计决策 |
| E5/E6 | — | — | **建议不做** |

顺序:E1 → E2 → E4 →(E3)。每项落地后在 golden 测试里加对应断言
(E1:bigram 场景断言;E2:recency 前后断言;E4:decomp 内部排序断言)。

## 关联

- 上一轮:issues-round6.md(卫生轮;D1 保留的 InputContext 管道就是
  为 E1 准备的,D6 能力矩阵的"缺口"就是 E2/E3)
- weight-scoring.md(分数来源对照表 —— E1/E2 的合成公式应同步登记)
- pinyin.md(lattice/成员层次)

## 执行记录

- **E1 ✅** 跨提交 bigram 联想落地:
  - `predict` 拆出 `predict_inner(input, prev_word)`,`lattice`/`lattice_prefix`
    分支在 freq→score 前施加 `bigram_boost(last_commit.0, 候选) × bigram_weight`
  - `PinyinWeights.bigram_weight`(1.0)+ yaml `weights.pinyin.bigram_weight`
  - `freq_to_score` 加 `min(max_score)` clamp(boost 可推 freq 过 recorded max)
  - `predict_with_context` 接线 prev_word(与 recency/整词联想同 gate)
  - 黄金断言 `bigram_context_lifts_co_occurring_word`(mini FST 三词设计:
    一律 130k 锚 + 异曲 60k/一起 50k,嵌入 bigram(我们,一起)≈26.9k 反超)
  - 标定注记:嵌入语料 (今天,天气)=0(无此对),(非常,好)≈35k,
    (我,们)/(的,时候)=50k 封顶 —— 常见对普遍在 15k~50k 区间
  - 测试:ime-core 154+21+2、swift-ime 7+10+15 全绿
