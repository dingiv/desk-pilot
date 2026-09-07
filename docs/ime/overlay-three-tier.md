# Overlay 三级架构(round19 设计稿)

> 状态:**已实施(round19)。** 参数经确认:阈值 128;频率/时间统计分表;三层词典同一查询方式(同一预测各查一次)。 前序:`overlay-dict-arch.md`(round17/18
> 两级体系:SeedDict + OverlayData/MemoryLayer)。本文将其升级为**三级**。

## 一、端到端需求

重复输入某些词之后,这些词在后续预测中得到增强;系统词典没有的自生词
也进入使用统计。Seed 词典(以及用户添加的词典)是"死"的、非用户定制的;
**Overlay 体系提供单用户专一优化**。

## 二、三级架构

```text
L1  OverlayData(内存,实时)         小而热:当前工作集
    · 每个新词进来 → 全套重建流程(重算有效频率、重排)
    · 量小,重建成本可忽略
    · 达到阈值 → 整体持久化到 L2,然后清空(数据已搬走)

L2  OverlayDict(持久化,冷加载)      大而稳:历史沉淀
    · 启动冷加载,**独立成层 —— 不再与 SeedDict 合并**
    · 直接作为预测候选来源之一(自生词即使不在 seed 也能出候选)
    · 会话内只读(被 L1 覆盖时不改写;下一次 flush 才吸收新状态)

L3  SeedDict(不可变)                rime-ice.fst + 用户添加的静态词典
```

查找优先级(同词多层命中时,**越热越权威**):

```text
L1 OverlayData  >  L2 OverlayDict  >  L3 SeedDict
   (实时增强)      (上次沉淀)         (原始频率)
```

## 三、与 round17/18 两级设计的差异

| 项 | 旧(两级) | 新(三级) |
|---|---|---|
| OverlayData 角色 | 永久累积,快照双写 | **工作集**:攒够即搬家、清空 |
| OverlayDict | memory 表快照,只做权重覆盖 | **独立预测层**,冷加载,直接出候选 |
| 合并 | MergedDict = Seed×Overlay 覆盖 | 三层逐级覆盖:L1 > L2 > L3 |
| 自生词来源 | PhraseBook(旁路) | L2 直接出候选(主路) |

## 四、关键流程

### 4.1 提交(写入路径)

```text
commit → L1 MemoryLayer.record_commit_weighted
         频率继承链:L2 命中 → 继承 L2 基础频率
                    否则 L3 命中 → 继承 SeedDict 原始频率
                    否则自生词 → SELF_GEN_FREQUENCY,并写 Wordbook
```

### 4.2 flush(L1 → L2,阈值触发)

```text
L1.len() ≥ FLUSH_THRESHOLD
  → 逐条 upsert 进 L2(同词:L2.count += L1.count,频率取
    max(L2 基础频率, L1 有效频率),last_ms 取新)
  → L1 清空
  → L2 全量快照落盘(SQLite overlay_dict 表)
```

flush 时刻的**规范化**:进入 L2 的条目以"基础频率 + 累计 count"形态
沉淀;有效频率(含 count 增强)在读取时派生 —— 存储不存派生值,与
round18 的无写放大原则一致。

### 4.3 预测(读取路径)

```text
候选来源:L3(现有 FST/lattice 路径,不变)
        + L2(pinyin == input 的词条直接出候选,freq_to_score 同刻度)
        + L1(工作集词条同上;量小,线性扫)
覆盖:同词多层命中 → 高优先层的有效频率换算运行时权重
微调:tier(recency + 频次增强)在覆盖后叠加(不变)
```

### 4.4 启动

```text
SeedDict 加载(不变)→ L2 冷加载(overlay_dict 表)→ L1 空
```

## 五、参数决议(已确认)

1. **FLUSH_THRESHOLD = 128**(`store/memory.rs` 常量;512 上限硬顶兜底)。
2. **频率统计与时间统计分表**(用户指令:"它的频率统计。和时间统计分开。
   那不是一张表")—— L1 内部两张独立 map(freq / recent),L2 落盘两张
   SQLite 表(`overlay_freq` / `overlay_recent`),flush 双表同步搬运。
3. **tier 三级穿透**:L1 时间表 miss → L2 时间表继续算档(刚沉淀的词
   不丢近期加成);`WordBook::tier` 统一入口。
4. **三层词典同一查询方式**(用户指令:"对于同一个预测,调用拼音家族的
   预测进行三次,分别针对不同的词典"):全拼精确命中 → (词, 有效频率),
   三层完全一致。
5. **关闭保底**:引擎 `Drop` 时 L1 无条件 flush 进 L2 并落盘 —— 未达
   阈值的会话数据不丢。

## 六、实施落点(已完成)

- `store/memory.rs`:MemoryLayer 重构为 freq/recent 双表;`flush_into`;
  `load_legacy`(旧 memory 单表迁移)+ `load_legacy_recent`(旧 recency 表)
- `store/overlay_dict.rs`(新):L2 结构 + `absorb`(count 累加/频率取大/
  时间取新)+ `lookup`(独立预测层)+ `tier`(穿透)
- `store/sqlite.rs`:新表 `overlay_freq` / `overlay_recent` + 读写
- `family/pinyin/merged.rs`:三级覆盖(L1 > L2 > L3)+ L1/L2 独立出候选
  (source = "overlay",上限 8,自生词主路)
- `store/wordbook.rs`:挂 `overlay_dict`;`maybe_flush`(阈值触发)+
  `flush_now`(Drop 保底)+ `tier` 穿透
- `store/manager.rs`:warm_all 冷加载 L2 两表
- 集成测试:引擎关闭 flush → L2 两表直接可读(频率继承非零);新增
  L2 单元测试 4 个(flush 合并/精确查询/穿透/roundtrip)

## 七、自生词进 lattice 预测(round19 续)

**缺口**:`LatticeDecoder` 只由 SeedDict FST 构建,自生词只走精确匹配
旁路(merged 注入)—— 连续长句拼音流的 lattice 切分里没有自生词节点。

**方案**:LatticeDecoder 挂 **overlay 旁路词典**(生成号驱动):
- `overlay: RwLock<OverlayMap>`(拼音 → (词, 基础频率, count))+
  `overlay_initials: RwLock<OverlayInitialsIndex>`(声母索引,Mixed/
  Initials 对齐用),`set_overlay_entries` 整体替换;
- 五个查询点全部与 FST 合并:`words_for`(单字区/继承/整词联想)、
  `predict` exact、`predict` Mixed/Initials 声母对齐、`predict_prefix`;
  同词双册时 overlay 有效频率胜出(越热越权威);
- 同步:L2 OverlayDict 加 `generation`(absorb/load 递增),家族侧
  `sync_lattice_overlay()` 比对缓存代,变了才重灌(避免每查询重灌);
  predict_inner / predict_chained 入口各调一次,warm 后主动同步一次。

CLI 实测:自生词(李正明,基础 100 + count 3)在精确全拼(`pinyin/
lattice`)、前缀联想(`lattice_prefix`)、声母简拼(`lattice_jp`,压过
seed 的来证明/老总们)三种模式全部出候选。

## 八、验证

217 测试全绿;clippy 双 crate 0;eval 持平 98.0 / 99.4。
