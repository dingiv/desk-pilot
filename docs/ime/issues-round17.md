# round17 — 多词典体系:SeedDict / OverlayDict / Wordbook / OverlayData / MergedDict

## 一、体系全景

```text
磁盘  SeedDict     rime-ice.fst(系统自带,22MB,拼音\x00词 → 词频)
      OverlayDict  memory 表的规范化快照(word, pinyin, last_ms, count, weight)
                   —— 周期性生成的时间策略后续设计(当前:提交即双写快照)
      Wordbook     WordBook(运行时实时同步的自生词:词 + 拼音 + 词频;
                   pinyin=PhraseBook(count), english=user_words(score))
内存  SeedDict / OverlayDict / Wordbook(同上,启动加载)
      OverlayData  MemoryLayer(实时动态 overlay;round16 所建)
      MergedDict   SeedDict × OverlayData 合并视图(family/pinyin/merged.rs)
```

## 二、OverlayData 权重来源(继承规则)

`MemEntry` 新增 `weight: u64`(0 = 尚无权威权重,如旧迁移种子):

- **词在 SeedDict**:`record_pick` 提交时查 lattice(FST)`words_for(pinyin)`
  取原始频率分**继承**为 overlay 权重;
- **自生词**:登记时赋固定默认 `SELF_GEN_WEIGHT = 100`(与 build_dict
  无权重 TSV 的缺省一致),同时记入 Wordbook(实时同步)。

→ OverlayData = SeedDict 权重继承 ∪ Wordbook 自生词,用户最新一手
输入词汇,由覆盖层统一管理权重。

## 三、MergedDict 权重覆盖算法

`family/pinyin/merged.rs::apply_overlay_override`,在 `predict_inner`
收尾(所有加成/bonus 之后、排序之前)施加:

- 候选词在 overlay 中有词条 **且** 拼音映射对 == 当前输入 **且**
  weight > 0 → `raw_score = freq_to_score(weight)`(与 FST 同刻度);
- **排序兼容的证明**:种子词继承的 weight == FST 原频率,同刻度换算
  → 分数与种子路径完全一致 → eval 零漂移(实测 98.0 / 99.4 持平);
  自生词(phrase 来源)则被规范进词频域;
- **两层语义不混**:overlay 定基线(权威权重),统计微调(recency
  tier / 频次增强)在覆盖之后照旧叠加。

## 四、持久化

- `memory` 表新增 `weight` 列(`ALTER TABLE` 迁移,旧库默认 0);
- `save_memory / load_memory / warm_memory / dump / load_bulk` 全链路
  5 元组;集成测试新增「种子词权重继承非零」断言。

## 五、验证

- 211 测试全绿;clippy 双 crate 0;评测持平 98.0 / 99.4
  (覆盖算法排序兼容性的直接证据)。
