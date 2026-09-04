# Overlay 词典与常态化学习 — 设计文档

> **状态:设计稿(2026-09-04),未实施。** 目标:让预测从"语料库的分布"
> 变成"这一个用户的分布"。分两个 Phase:**Phase 0 词典大清洗**(前置,
> 语料频率维必须真实,overlay 的嫁接才有意义)→ **Phase 1/2 Overlay 词典**
> (用户频率维 + 用户上下文维)。

## 一、动机与现状缺口

现有五套学习机制全部"事件驱动 + 苛刻条件",没有任何一套回答
"**这个用户历史上常用哪些词**":

| 机制 | 门槛 | 缺口 |
|---|---|---|
| PhraseBook 自造词(`pinyin/mod.rs phrase_score`) | 必须走数字键逐字造词流程 | **直接空格选词不学**(weight-scoring.md 明确豁免:decomp 词不进本) |
| recency(`pinyin/recency.rs`) | 1 次即记 | 纯时间衰减,3 天过期;无频率维度、≤512 行 |
| L0 pins(3 选自动 pin) | 3 次 | 二值钉住,非统计 |
| bigram(`dict.bigram_boost`) | 语料共现 | 相邻词对,**语料的**上下文,不是用户的 |
| en_user(raw 提交) | Enter 强选 | 只收英文自造词 |

核心缺口:**用户直接空格选中的词典词(占日常提交的绝大多数)零统计**。
引擎记录的是"语料库认为什么常用",不是"这个人常用什么"。

## 二、Phase 0 — 词典大清洗(前置硬依赖)

### 2.1 数据质量问题清单(实测证据)

rime-ice(91.6 万条 → `rime-ice.fst` 22MB)的 weight **两种量纲混用**:

| 词条类型 | weight 来源 | 实测 |
|---|---|---|
| 单字(8105 字表) | 真实语料**字频** | 的 76,938,354 / 一 35,278,860 / 是 31,422,712 |
| 多字词组 | 词库作者**手工标注等级** | 顶流"版权"13,204,281、**"江泽民"19,260,817(人工抬顶)** |

后果(全部在 round10 评测中实测暴露):

1. **量纲断崖 58 倍**:词频嫁接(W1 词频驱动 single、E1 bigram 嫁接)
   在跨量纲时失真——单字真实词频压倒一切,多字词组内部却是手工序;
2. **同档同分**:及时/即使(jishi)、出示/初始(chushi)Top-1 二选一纯碰运气
   (手工标注同等级,round10 执行记录遗留项);
3. **噪声词占池**:人名(杜淳/张翰/蒋钦)、诗性词(度春宵/骀荡)、拟声重复
   (啊啊啊 ×4)、网络词以不低的 weight 挤占候选池(round10 评测 57 个
   Top-1 miss 的主要成分);
4. **政治/敏感/过时词条**随词库带入(见上"江泽民"权重顶流)。

### 2.2 清洗数据源

**OpenSubtitles 中文词频**(hermitdave/FrequencyWords,与英文侧
`hermitdave.tsv` **同源同格式**,英文侧 round10 W2 已验证这套数据质量):

- `zh_50k.txt` / `zh_full.txt`:`词 count` 两列,**已分词的真实字幕语料频
  率**,单字与词组**同一量纲** —— 直接解决量纲断崖;
- 获取:新脚本 `scripts/fetch_zh_freq.sh`(照抄 `fetch_emoji.sh` 的
  下载+转换模式),入库 `assets/dict/zh_freq.tsv`。

### 2.3 清洗管线(`scripts/refine_dict.sh`,可重复执行、规则可配、不手工)

```
rime-ice TSV ──┐
               ├─ refine_dict → cleaned TSV → build_dict → rime-ice.fst(v2)
zh_freq.tsv ───┘
```

规则(按序,每条独立开关,输出统计报告):

1. **语料频率覆盖**(核心):`word ∈ zh_freq` → `weight = 语料 count`
   (量纲统一;单字/词组同源)。预计覆盖常用词的绝大多数;
2. **未覆盖词条降置信**:不在语料表 → 保留原 weight × 0.05 缩放
   (保留可查但沉底),并打 `# @uncalibrated` 统计;
3. **拟声/重复词过滤**:`A+` 模式(啊啊/啊啊啊/哈哈哈哈)weight × 0.1;
4. **噪声词降级**:纯 ASCII 夹杂、超长(≥8 字)非成语词条删除;
5. **敏感/人工抬顶词条**:维护一个显式删除清单文件
   (`assets/dict/blocklist.txt`,人工审阅制,不自动判)。

产物:新 `rime-ice.fst`(build_dict 现有工具链直接复用)+ 清洗报告
(条数/覆盖率/删除清单)。`.fst.idx` 缓存随 FST mtime 自动失效重建。

### 2.4 Phase 0 验收

- `jishi → 及时 #1`、`chushi → 出示 #1`(真实语料序);
- tc_dict_sample(789 条)Top-1 ≥ 98%(round10 为 97.5%);
- 人名/诗性词类 miss 显著下降;`江泽民` 级词条不再出现于前排。

## 三、Phase 1 — Overlay 词典(用户频率维)

### 3.1 记录:上屏即 +1

记录点**已存在**:`FamilyPipeline::commit_text`(fsm/family.rs:670)是
一切真实上屏的统一出口,`env.record_commit_text(word, family)` 钩子现成
(engine.rs:835 现只做 recency 分派)。

- **记**:空格选词、数字选词、英文 exact 提交 —— 词典词与自造词一视同仁;
- **不记**(豁免表):`family == None` 的模板类提交(snippet 展开/魔法
  命令/语音整段)、Enter raw 强提(未选词)、`#del`/`#clip` 类命令产物。

### 3.2 存储

```sql
CREATE TABLE IF NOT EXISTS overlay (
  word TEXT PRIMARY KEY,
  pinyin TEXT,        -- 最近一次提交拼音(参考用,非查询键)
  count INTEGER,      -- 累计使用次数(永久)
  last_ms INTEGER     -- 最近使用时间(展示/诊断)
);
```

- 写穿:提交时 UPSERT `count+1`(SQLite WAL,提交频率低,毫秒级);
- warm:`init_store` 后全量载入内存 `HashMap<String, (u64, u64)>`
  (热路径 O(1);5 万行 ≈ 数 MB,可接受);
- 上限:软告警 10 万行(超出仅告警,不自动淘汰 —— 用户习惯不应被清理)。

### 3.3 合成:词频嫁接(与 E1 bigram 同构,不发明新刻度)

在 `predict_inner` 的 lattice/single 候选循环内(E1 同位置):

```
overlay_boost(count) = 50_000 × ln(1+count) / ln(1+1000)
boosted_freq = dict_freq + overlay_boost(count) × weights.overlay_weight
score = freq_to_score(boosted_freq)        # 与 lattice 同一条映射,自然封顶
```

- **覆盖语义 = 只升不降**:词典原分不动,overlay 只做加法;
- 曲线容错:误选 1~2 次 boost≈数百等效词频,不改变排序;30 次 ≈ +25k、
  100 次 ≈ +37k、封顶 50k —— **远小于清洗后的语料顶流**(亿级),用户的
  习惯词升到"高频档",但永远压不过"的/了"级超高频,无需担心霸榜;
- PhraseBook 自造词同样吃 boost(词表维 + 频率维自然叠加);
- bare 引擎(无 FST 词频域):overlay 命中词直接以
  `freq_to_score(boost)` 作为该词分 —— 语义等价(全部词频都来自用户)。

### 3.4 与 recency 的关系(正交两维,明确分工)

| | overlay(Phase 1) | recency(现有) |
|---|---|---|
| 维度 | **频率**("历史上常用") | **时间**("刚用过") |
| 衰减 | 无(永久累计) | 五档时间指数,3 天过期 |
| 门槛 | 曲线自然(ln) | 1 次 |
| 数据 | overlay 表(count) | recency 表(≤512 行) |

**合成链(单向,不互相感知)**:

```
dict_freq ──(+overlay_boost)──► freq_to_score ──► raw_score
                                 ──(recency z 合成)──► 最终分
```

- overlay 先行(家族内,嫁接进词频),recency 后行(排序前的 z 合成,
  现有 `apply_recency` 位置不变)—— 两层各自封顶,无过冲;
- 常见疑问"30 天没用的词 boost 还在,会不会压新词?"——会,但这正是
  "常态化"的语义(年度报告词一年用一次,每次都想要它);新词靠自己的
  overlay 累积 + recency 短期 boost 竞争。若实测确实僵化,再考虑对
  `last_ms` 超一年(半衰)的词条打折 —— **列为观测项,先不做**。

### 3.5 上下文感知(Phase 2 — 用户 bigram)

现有上下文全是**语料的**(语料 bigram、context_comp 整词联想)。Phase 2
补用户维:

- 新表 `user_bigrams(prev TEXT, next TEXT, count INTEGER, PRIMARY KEY(prev,next))`;
- 记录:同 `commit_text`,与 overlay 同点(上次提交词 = prev);
- 合成:与语料 bigram **同点同量纲相加**:
  `total_boost = corpus_bigram(prev,next) + user_bigram_boost(count)`;
- gate:沿用 `input.context_aware` 统一开关(含语料与用户两层)。

### 3.6 配置面

```yaml
input:
  overlay_learning: true     # false = 不记不用(隐私开关)
weights:
  overlay_weight: 1.0        # 嫁接系数;0 = 只记不合
```

### 3.7 隐私与诊断

- overlay 是**本地行为记录**(本地 SQLite,不外传,不进任何日志);
- `swift-cli --overlay-stats`(后续):count Top-N / 总词条数;
- `--reset-overlay`(后续):清表重来。

## 四、分期计划

| Phase | 内容 | 验收 |
|---|---|---|
| **P0** | 词典大清洗(refine_dict 管线 + zh_freq 语料) | §2.4 三条 |
| **P1** | overlay 表 + commit 记录 + 词频嫁接 + 配置 | 场景:提交"部署"×5 → `bugu` 部署稳 #1;全量 eval 无回归 |
| **P2** | user_bigrams + 语料 bigram 同点合成 | 场景:常打"部署"后输 "huanjing" → 环境 提前 |

## 五、风险

- **清洗误伤**:zh_freq 语料(字幕口语)覆盖不了书面词(成语/术语)——
  降置信(×0.05)而非删除;blocklist 人工审阅;
- **隐私**:overlay/user_bigrams 是行为画像 —— 本地存储、隐私开关、
  reset CLI 三件套;
- **性能**:热路径 HashMap 查询 O(1);每提交一次 UPSERT(低频,毫秒级);
- **测试**:round 计划里既有测试全绿为硬约束;清洗 FST 后
  rime_ice_smoke/global_ranking 两条 golden 线必须逐条复核(权重域整体
  移动,可能出现需按新词频修订的断言 —— 修订须在清洗报告佐证下进行)。

## 关联

- 前置事实:round10(W1 词频驱动 single / W2 hermitdave 并入 / E1 bigram
  嫁接先例);weight-scoring.md 的分数来源对照表
- 用户原话:引擎必须记录用户输入的每一个词(含词典词与自造词),统计形成
  overlay 词典,覆盖在已有词典之上,极大重塑预测结果,符合单一用户习惯;
  先清洗词典(当前词典数据太差)。
