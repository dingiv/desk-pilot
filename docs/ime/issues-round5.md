# swift-ime 第五轮 — 架构接缝修复

> 创建: 2026-08-15,**已完成**(同日)。源于对家族路由/打分体系的一次全面 review(见
> [weight-scoring.md](weight-scoring.md) 的层次表)。

## 动机:问题都在接缝处

第四轮以来的每次排序修复(jix/jixu 同分、emoji 压英文本尊、闹着压闹钟、
机械的压继续)都是同一模式:**两个家族的手调常数在分数空间意外相撞**。
跨家族的预期顺序(全拼 > en exact > emoji exact > …)没有任何一处代码
声明——它是 ~15 个常数的涌现结果。每次调参(如 `family_priority.pinyin
100→80`)都可能打破某个看不见的边界。

同时积累了三块**写而不读**的死重,和三套语义微妙不同的"前缀+衰减"实现。

## 设计

### 1. 全局排序黄金测试(止血,优先)

一张"代表性输入 × 预期顺序"断言表,**锁顺序不锁分数**:

| 输入 | 断言 |
|---|---|
| `jixu` | 继续 > 急须(全拼区分度)|
| `clea` | clean(english) > 🫧(emoji 前缀)—— 本尊压过同名词 emoji |
| `naozh` | 闹钟 > 闹着 —— 高频远词压低频近词 |
| `jix` | 继续 #1 —— SCAN_CAP/去重方向回归 |
| `cd` | 承担(jianpin) > 📀(≤2 字母 emoji 降权)|
| `swift` | swift(en exact) > swifts(en prefix)|
| `smile` | smile(en exact) > 😊(emoji exact)|
| `de` | 的(single)> emoji/英文 |
| `name` | 那么 > name(en)—— 且 name **不得**是 pinyin/phrase(纯 ASCII 拒学)|

用**默认权重**构造引擎(测试不走 swift-ime.yaml,不受用户调参影响)。
任何常数调整打破层次 → 立刻红灯,不再靠手测发现。

### 2. 统一前缀衰减 helper

现状三套实现:

| 家族 | 公式 | 备注 |
|---|---|---|
| pinyin | `freq × prefix_lookup × 0.85^(d−3)` | d=联想词拼音−输入,免费额度 3 |
| emoji | `0.6 × 0.85^(d−3)` | 同额度 3,另有 ≤2 字母关键词降权 |
| english | `0.60 + 0.25 × 词频 × 匹配率` | **语义不同**:质量式,无距离衰减 |

收敛:pinyin/emoji 的**相同部分**(0.85 底数、额度 3)提取为
`scoring::prefix_decay(diff)`;english 的质量式公式保留(它要表达"词频+
匹配率",不是"距离可信度"),在 helper 文档里标注差异。黄金测试守住三者
的相对层次。

### 3. 死代码清理(减负)

| 项 | 链路 | 处置 |
|---|---|---|
| **UserBigram** | 引擎每次提交写入(SQLite + 内存)、启动 warm——但 `boost()` 生产零调用(已被前缀整词联想替代) | 全链路删除:`user_bigram.rs` 模块、family 字段、trait/dispatcher/engine 方法、persistence warm、WeightStore bigram 方法;SQLite 旧表留在库里不迁移(无害)|
| **surrounding text** | C++ **每键**拉取 → C ABI → engine → InputContext.surrounding——predict 已不消费 | 删 C++ 拉取块、ABI 函数、engine setter、InputContext 字段 |
| **pins 表** | `pin_word` 生产零调用,`pin_count` 仅启动日志 | 删方法,启动日志改用 phrase/en_user 计数 |

### 4. 文档纠正

`large_dict` **不是**死参数——它是 `single` 音节候选的基础分
(`de→的` 的 0.85 来自它)。weight-scoring.md 更正说明;bigram 相关
章节随删除同步移除。

## 行动计划

| # | 任务 | 规模 | 状态 |
|---|------|------|------|
| 1 | 黄金测试(`apps/swift-ime/tests/global_ranking.rs`,9 用例) | ~200 行 | ✅ |
| 2 | `scoring::prefix_decay` 收敛(pinyin/emoji 共享) | ~30 行 | ✅ |
| 3 | 死代码清理(UserBigram 全链路/surrounding 全链路/pins;净删 ~330 行) | — | ✅ |
| 4 | weight-scoring.md 更新(large_dict 纠正/衰减统一说明) | 文档 | ✅ |

**不做**(评估过,收益/成本不划算):
- 分数层次声明化(大改架构;黄金测试已能兜底)
- 提交事件统一(等下次加学习类型时顺手做)
- en_user 频率曲线(固定 0.88 无实际痛点)

---

> **第五轮收尾。** 非魔法家族(pinyin/english)预测逻辑的新一轮检视见 [第六轮 →](issues-round6.md)
