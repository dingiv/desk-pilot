# swift-ime 第八轮 — 预测流程规范化(三阶段管线)

> 创建: 2026-08-30。用户定义的目标架构:每个 key 的 route 动作分为
> **stage1 系统控制 / stage2 家族预测 / stage3 后处理** 三个阶段。
> 本轮先规划,再分步实施。

## 一、现状管线(核对过的事实)

```
route_inner (router.rs:300)
├─ Ctrl/Alt → passthrough                                  ← 系统控制
├─ Snippet 态 command_char → sm.step                        ← 系统控制(命令文本直通)
├─ match key.kind:
│   ├─ Space/Enter/Backspace → sm.step(提交/强选/删除)      ← 系统控制
│   ├─ Digit → 数字选词 / 透传                               ← 系统控制
│   ├─ PageUp/Down/±/= → change_page(翻页状态机完整)        ← 系统控制
│   ├─ Up/Down → move_highlight(highlight 推导 page)        ← 系统控制
│   └─ Char(c) → sm.step(c) → 状态三分:
│       ├─ Idle    → handle_idle(# 触发 snippet / 字母进 Pinyin / 透传)
│       ├─ Snippet → handle_snippet → query_magic(预测+组装混在一个函数)
│       └─ Pinyin  → handle_pinyin
│           ├─ 字母 → buffer 累积 → query_pinyin            ← 二三阶段交错
│           └─ 分隔符/数字 → pinyin_terminator(提交/选择)   ← 系统控制
│
query_pinyin (state.rs:1081) —— 现状:stage2/stage3 无边界地混在一起:
├─ ranked = scorer.rank_detailed(buffer, ctx)   ← 家族循环:predict_with_context
│                                                 → 各家 sort/top_n → ×priority
│                                                 → 全局 sort/去重
├─ promote_single_letter(buffer, ranked)        ← 后处理
├─ last_meta = ranked                           ← 后处理(meta 采样,在重排前!)
├─ Layer 3 造词单字区(merged 重排)              ← 后处理
└─ make_view → fill_view(翻页滑动窗口)          ← 后处理(视图)
```

## 二、三阶段定义

### Stage 1 — 系统控制(System Control)

- **输入**:KeyEvent + FSM 状态(Idle/Snippet/Pinyin + flags)
- **职责**:回答"这枚键是不是预测问题"。提交(Space/Enter)、强选/删除
  (数字/Backspace)、导航(方向/翻页)、退出(Escape)、命令触发(#),
  以及一切透传判定。**消耗键即返回 view,不产生预测**。
- **所在**:`router.rs::route_inner` + `state.rs` 的 handle_* 非预测分支
  + pinyin_terminator。
- **规范**:判定表显式化(键 × 状态 → 动作的矩阵,route 头部注释文档化)。

### Stage 2 — 家族预测(Family Prediction)

- **输入**:规范化 buffer(拼音/字母串)+ InputContext
- **职责**:各家族**独立**产出 `Vec<ScoredCandidate>`。
  - **内部无需感知**:家族自己的成员层次(lattice/decomp/phrase/recency/
    bigram(E1)/self_case/user/en recency(E2)/chained)全部封装在家族内,
    context_aware gate 也是家族内部自决。
  - **外部无需感知**:调用方只见 trait —— `predict(input, ctx) → Vec<ScoredCandidate>`。
  - **家族间零耦合**:英文不知道拼音存在,反之亦然。
- **所在**:`family/*`(现状即如此)+ `CandidateFamily` trait。
- **规范**:trait 收敛**单入口** `predict(input, ctx)`(predict 与
  predict_with_context 双入口合并,无 ctx 传 `InputContext::new()`)。

### Stage 3 — 后处理(Post-Processing)

- **输入**:各家候选(raw)+ FSM 上下文(buffer/first_syllable/状态标志)
- **职责**(统一管线,**单一实现**):
  1. 合成:×priority、去重、全局排序(现 rank_detailed 的合成段)
  2. 全局调整:promote_single_letter
  3. 造词单字区(Layer 3 重排)
  4. meta 采样(last_meta —— **必须在重排后**,与 merged 同序)
  5. top_n 截断
  6. 视图组装(make_view / fill_view 翻页滑动窗口)
- **输出**:`self.candidates` / `last_meta` / `partial_commit_indices` + ImeView
- **所在**:从 query_pinyin 拆出的独立后处理段(state.rs 内私有函数起步,
  视规模独立成 `fsm/postprocess.rs`)。

## 三、差距与动作清单

| # | 现状 | 目标 | 动作 | 风险 |
|---|---|---|---|---|
| A | query_pinyin 二三阶段交错(120 行大函数) | rank → PostProcess 两段清晰切分 | 抽取 | 低 |
| B | `candidates_detailed` 镜像独立重算(rank_detailed + promote,**无 Layer 3**/无造词单字区) | 镜像改走同一条 stage3 管线 | 消双路径 | 中(调用点审计) |
| C | last_meta 采样在 Layer 3 重排**前** → fill_view 的 `last_meta.get(start+i)` 在重排后错位(单字区 meta 显示错的来源) | 采样移到重排后,与 merged 同序 | **真 bug 修复** | 低 |
| D | trait 双入口 predict / predict_with_context | 合并单入口 predict(input, ctx) | trait 收敛 | 低 |
| E | route 判定散在 match 分支 | 判定表文档化(route 头部) | 文档 | 无 |
| F | query_magic(snippet 态)预测+视图组装混在 state.rs | **决策点**:magic 家族化纳入 stage2,或保持现状标注豁免 | 待拍板 | 中(若做) |
| G | scorer 的合成段(×priority/sort/去重)归属模糊 | 归 stage3(合成是后处理第一步);scorer 保留为数据结构,合成逻辑移出或标注 | 边界定义 | 低 |

## 四、实施计划(每步独立提交,golden 全程护航)

| 步 | 内容 | 规模 | 依赖 |
|---|---|---|---|
| S1 | **trait 单入口**(D):predict_with_context 并入 predict(input, ctx);调用方(rank_detailed)单点调用;context_aware gate 家族内自决(已是) | 小(~50 行) | 无 |
| S2 | **后处理抽取**(A+C):query_pinyin 拆两段;last_meta 采样移到重排后(修 C 的 meta 错位);PostProcess 单元测试 | 中(~120 行) | 无 |
| S3 | **镜像统一**(B):candidates_detailed 改走同一条 stage3 管线(读 FSM 的 last_meta/candidates,或同一 PostProcess 重放);golden 的 rank() helper 从此与用户真实候选同源 | 中 | S2 |
| S4 | **判定表文档化**(E)+ magic 豁免标注(F 决策) | 小 | S3 |
| S5 | (可选)**magic 家族化**(F):query_magic 的预测段抽成 MagicFamily 的 stage2 参与 | 中 | S4 拍板 |

## 五、不变式(全程护航)

- `global_ranking` 12 项 golden 断言顺序不变
- Space/digit 提交行为不变(select 的全局序语义不变)
- view.candidates 翻页滑动窗口语义不变(page_size 构造参数链路不变)
- context_aware 开关行为不变(仍是家族内 gate)
- 造词单字区/partial commit 交互不变(">"/部分提交)

## 六、风险与已知行为变化

- **C 的 meta 修复是行为变化**:Layer 3 重排后单字区的 meta 注释会变成
  正确来源(此前错位)——修复而非回归
- **S3 后 detailed 与真实候选同源**:candidates_detailed 的输出会**多出**
  造词单字区(此前没有)——golden rank() 的断言环境更真实,个别断言
  可能需要复核(top1 断言不受影响,any/position 类断言复核)
- S1 的 trait 签名变化波及所有家族实现 + 测试 mock(机械改动)

## 关联

- 上一轮:issues-round7.md(E1/E2/E4 + dier/造词/翻页修复——本轮的
  stage2 内聚与 stage3 双路径问题都是那几轮放大出来的)
- weight-scoring.md(stage3 合成公式的登记处,合成段移动时同步)
- fsm/mod.rs 的路由注释表(route 判定表文档化的落点)

## 执行记录

(待填:S1-S5 每步落地后登记提交与断言)
