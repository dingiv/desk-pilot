# swift-ime 迭代轮次历史(round 2–19 汇编)

> 本文件整合原 `issues-round2.md` ~ `issues-round17.md`(已删除)。
> 每轮一条:目标 / 关键决策与产出 / 验证。设计文档见同目录
> `design.md` / `pinyin.md` / `weight-scoring.md` / `overlay.md` /
> `fcitx5-integration.md` / `input-router.md`。

## 当前基线(每轮验收标准)

- `cargo test -p ime-core -p swift-ime` 全绿(当前 217 passed)
- `cargo clippy` 双 crate 0 警告
- 评测持平:tc_dict_sample Top-1 **98.0%** / tc_en_sample **99.4%**
  (在 `apps/swift-ime` 下 `--cases` 跑,纯净状态:移开 `data/swift-ime.db`)

---

## round 2(2026-07-31)— 权重体系 + 统一 Lattice

- **修复**:fetch_dict.sh 曾砍掉 rime-ice weight 列导致全员同分;恢复 3 列,
  `LatticeDecoder::freq_to_score()` log₂ 归一化 [0.25, 0.90],虚词黑名单 +
  `stopword_penalty`。
- **统一 Lattice**:`dict` + `viterbi` + `jianpin` 三个 member 合并为
  `LatticeDecoder`(全拼 FST O(1) / 简拼混写 initials_index + pattern_match)。
- 验证:Top-1 87.5% / Top-3 100%(16 条手测)。

## round 3(2026-07-31)— 用户自定义词典 FST

- 多 FST 并查设计(`my_words.fst` 与系统词典同待遇:频率排序 + 简拼/混写),
  初步落地用户词典构建链。

## round 4(2026-08-03)— 上下文感知 + 中英混输

- 上下文感知预测(short-term recency + 整词联想 `context_comp`)、
  中英智能路由、English FST 词条层立项。

## round 5(2026-08-15)— 架构接缝修复

- **全局排序黄金测试**(`global_ranking.rs`):锁顺序不锁分数,止血
  "两个家族手调常数在分数空间相撞"的回归模式。
- 统一前缀距离衰减 helper(`prefix_decay`,pinyin/emoji 共享);死代码清理。

## round 6(2026-08-29)— pinyin/english 预测逻辑检视(只检视不修)

- Bug 清单:B1 `.en_cache` 重载损坏归一化、B2 双 pinyin 引擎脑裂、
  B3 家族开关死旋钮;规范问题:D1 InputContext 幻觉管道、
  D3 打分公式三套并存、D5 trait 肥胖、D6 中英能力不对称未声明。
- 修复在后续轮次消化。

## round 7(2026-08-30)— 中英预测体验强化

- **E1 跨提交 bigram 联想**:`bigram_boost(prev,next)` 词频量纲加成,
  freq_to_score 之前嫁接,gate `input.context_aware`。
- **E2 英文 recency**:复用拼音侧 RecentStore 公式(z 合成);英文前缀
  频率四档 band。E4 Viterbi 死权重激活。

## round 8(2026-08-30)— 预测流程规范化:三阶段管线

- 确立 **stage1 系统控制 / stage2 家族预测 / stage3 后处理**三阶段语义;
  StateMachine 状态下沉。

## round 9(2026-08-31)— FSM 文件结构重排

- 三阶段三文件显式命名:`pre.rs` / `family_prediction.rs`(stage2)/
  `post.rs`(stage3);**依赖单向化**:family → fsm 反向依赖清零。

## round 10(2026-09-01)— 预测精准度大改版(评测驱动)

- 首建大规模客观评测(词典分层抽样 789 + 英文 170 条)。
- **W1** 单音节词频驱动(single 分数 = FST 单字词频同刻度);
  **W2** hermitdave 高频表并入英文词条层;**W3** 英文 exact 词频化
  (`exact + exact_quality × band`);**W4** 前缀联想条件折扣(有 Full
  精确命中时再折一次)。
- 验证:pinyin 92.8 → **97.5**,english 77.1 → **99.4**。

## round 11(2026-09-05)— 预测流水线三阶段规范化(结构轮)

- 三阶段分工明细化:Stage1 收敛、Stage2 拆文件、Stage3 归位、单一候选
  路径。不改行为,评测持平(98.0/99.4)验收。

## round 12(2026-09-06)— 边界规范化 + stage2/stage3 解耦

- 壳内越界收口、**事件驱动模型**(不再靠轮询)、engine.rs TODO 处置、
  **stage2/stage3 交付件解耦**(stage3 拥有交付件定义,`FamilyPipeline`
  类型清零,post.rs 内 `impl StateMachine` 清零)。

## round 13(2026-09-06)— ControlPane 三大事件分发 + 双路键处理

- 用户模型落地:状态机 → `ControlPane` 分发三大事件;双路键处理
  (第一路 FamilyPrediction / 第二路 MagicFlow);封装 `ImeView` 返回。

## round 14(2026-09-06)— 学习功能出家族,归后处理

- **CommitReceipt**(stage2 产出)→ **post::learn_commit** 统一结算;
  家族不再感知 commit(干净状态机:内部状态 + 外部 context → 预测)。
- **WordBook 抽取**为独立模块(`store/wordbook.rs`),所有权归
  PersistenceManager。

## round 15 — 链式预测 × 魔法异步

- `abc'#asr'#translate` 场景:**ChainFlow** 部分重算(上游缓存命中零重算,
  语音段起下游重预测)+ magic_tick 分流 + 节流/防抖闸门;5 个时序单元
  测试钉死闸门时序。

## round 16 — 统一记忆模块层 MemoryLayer

- `store/memory.rs`:提交映射对(word → pinyin/last_ms/count/frequency)
  统一收拢 recency / 自生词统计 / 上下文感知三模块;SQLite `memory` 表
  持久化 + 旧 `recency` 表迁移;旧提交曾被外部重置,内容后来收编。

## round 17 — 多词典体系

- SeedDict / OverlayDict / Wordbook / OverlayData / MergedDict 五概念确立;
  **MergedDict 权重覆盖**:种子词继承原频率 → 同刻度换算分数不变 →
  排序兼容(eval 持平即证明);`MemEntry.weight` → **`frequency`** 更名
  (词典域频率,非运行时 0..1 浮点;round18 完成)。
- **round18**:频次增强 `effective = freq + min(count×10, 5000)`
  (读取时派生,无写放大);存储只存基础频率。

## round 19 — Overlay 三级架构 + 自生词进 lattice

- **三级**:L1 OverlayData(内存工作集,阈值 **128** flush 后清空)>
  L2 OverlayDict(SQLite 持久化,冷加载,独立预测层)@ L3 SeedDict
  (不可变);覆盖优先级 L1 > L2 > L3(越热越权威)。
- **频率统计与时间统计分表**(用户指令):L1 内部 freq/recent 双 map,
  L2 落盘 `overlay_freq` / `overlay_recent` 两张 SQLite 表。
- tier 三级穿透(L1 miss → L2 时间表);引擎 `Drop` 无条件 flush 保底。
- **自生词进 lattice**:`LatticeDecoder` 挂 overlay 旁路词典(生成号驱动
  同步),五个查询点与 FST 合并;自生词在精确全拼/前缀联想/声母简拼
  三种模式全部出候选(实测)。
- 存储审计:`en_cache` 归位 FileLoader DATA 命名空间。

## round 20 — recency 连续化 + 造词单字区后靠

- **真词头窗口 4→10**(`fsm/post.rs COMPOSE_HEAD_WORDS`):多音节输入时
  多字词整页优先于造词单字区(yibu→异步 #38→#6)。
- **recency 连续衰减**:近期指数从五档阶梯(10s/1h/5h/1d/3d 同档同分)
  改为连续指数衰减 `b(t)=min(5, 5×2^(−t/18h) + (count/3).min(1))`,
  超半衰期减半;频次增强 ≥3 阶跃 +1 连续化为 count/3 线性;三层
  (memory/overlay_dict/wordbook `tier`)与 pinyin/english 两个消费点
  同步改 f64。z 合成公式不变。
- 验证:218 测试全绿(+2:连续衰减/同档可分);eval 持平 98.0/99.4;
  CLI 实测提交 nihao→你好后 0.705→0.915(显示 ×家族优先级 0.8=0.732)。
- **round21 续 — 单公式合并**(同日):三段链(阶梯 b → z 合成)并为
  `g = 0.70 × exp2(−t/18h)`,`score' = a + (1-a) × g`;删除三个
  `tier()` 管道(复用 recent_ms 穿透语义)、count 双计(count 只在
  effective_frequency 一处)、powf→exp2 直取(省内部 log2 换算)。
  不变式钉死:>3d 惰性移出只删时间表,频率表统计不动。验证 218 全绿,
  eval 持平,CLI 复测 0.572→0.729(与手算一致)。

## round 22 — overlay 频率增量账本 + #freq 手工微调

- **22a 账本核心**:FreqEntry{base, delta, count};base 不可改写(种子
  继承 / 自生词中频 30k);提交记调和级数正笔 δ=8000×rel/(count+4)
  (对数增长自然涌现,有机封顶 50k);rel = count/全体均值 ∈[0.5,2.0]
  (相对增量:冷库重度偏好满加成、热库温和;他人使用→自己 rel 走低 =
  有机负向);effective 夹 [base×0.5, base+100k]。absorb 改 delta 累加;
  overlay_freq 加 delta 列(幂等迁移)。
- **22b #freq/up|down**:链式 `yibu'#freq/up` —— 分链时捕获面板高亮词
  (chain_anchor)提为上游首选,ChainContext 新增 root_text(拼音根);
  freq 成员 wants_context,对 (yibu↔异步) 记 ±25k 手工账,预览
  `异步 ↑ 词频 before→after`,空格提交该词;幂等防重复记账。
- 验证:223 测试全绿(端到端:yibu 高亮异步 → '#freq/up → 提交 →
  异步 top-3);clippy 0;eval 持平 98.0/99.4。
