# swift-ime 第十一轮 — 预测流水线三阶段规范化

> 创建: 2026-09-05。用户目标:"将三个阶段(系统控制预测 / 家族预测 / 后处理)
> 的分工和模块明细化、规范化拆分,减少代码冗余和模糊不清的地方"。
> 本轮是**结构轮**:不改行为,验收 = 全测试绿 + 评测持平(98.0/99.4)。

## 〇、现状盘点(代码为准)

round9 已建立三阶段骨架,`fsm/` 按阶段分文件:

| 阶段 | 文件 | 行数 | 内容 |
|---|---|---|---|
| Stage1 系统控制 | `fsm/state.rs` + `fsm/pre.rs` + `fsm/key.rs` | 431+203+359 | StateMachine(表壳)、ControlStage(键路由)、键枚举/状态位 |
| Stage2 家族预测 | `fsm/family.rs` + `fsm/chain.rs` | **1456**+60 | FamilyPipeline(组合会话聚合体)、链式段解析 |
| Stage3 后处理 | `fsm/post.rs` + `family/mod.rs`(merge) | 468 | 合成、过滤链、造词单字区、视图组装 |

下游:`family/mod.rs` 的 `UnifiedScorer`(collect=stage2 收集 / merge=stage3 合成)、
`family/{pinyin,english,emoji,magic,scoring}` 各家族;`engine.rs` 是壳(StepEnv 实现)。

## 一、问题清单(编号 P11-*)

### Stage1 系统控制

- **P11-1 action 归一化双份**:`ControlStage::route_key` 尾部(pre.rs:34-38)与
  `StateMachine::step` 尾部(state.rs:81-85)是**完全相同**的两段
  (NONE→HANDLED + flags 同步)。StateMachine.step 仅转调 ControlStage ——
  双重归一化,StateMachine 退化成壳(只有 flags 镜像 + control 成员)。
- **P11-2 stage1 伸手 stage2 内部**:pre.rs 直接读写 `pipeline.panel.page /
  page_size / items`、`pipeline.comp.cursor / preedit` —— 页内序号计算
  (digit 选词)、光标 clamp(`[` `]`)是面板/组合的行为,散在 stage1。
- **P11-3 stage2 的方法挂在 stage1 文件**:`FamilyPipeline::state_flags()`
  与 `change_page()` 的 impl 块在 state.rs:92-139 —— 状态派生是 stage2 语义。
- **P11-4 权威文档与实现分离**:路由决策矩阵(16 行表格)注释在 state.rs
  模块头,实际实现在 pre.rs。

### Stage2 家族预测

- **P11-5 1456 行巨型文件**:三个会话聚合体(Composition / CandidatePanel /
  MagicSession)+ 三态键处理(idle/snippet/pinyin)+ 链式命令全逻辑 +
  14 个 view helper 挤在一个文件。MagicSession 链式命令
  (query_magic / query_chained_magic / eval_upstream / eval_command /
  select_magic / force_fire)约 450 行,是独立会话语义,应自成模块。
- **P11-6 "取缓冲-重置-提交"六处手写变体**:select_magic 尾部、force_fire
  尾部、snippet_key Enter、pinyin_enter、pinyin_space(非 fresh)、
  pinyin_terminator —— 同一模式(`take` + `reset` + `commit_text` +
  `commit_view`)各写一遍,语义差异(family 参数、大小写回填)无结构表达。
- **P11-7 preedit 同步八处重复**:`preedit = committed_text + raw_buffer;
  cursor = preedit.len()` 在 pinyin_char / pinyin_backspace / select(partial)
  / handle_idle / query_magic 回退 / snippet 分支等 ≥8 处逐字重复。
- **P11-8 链式求值嵌套完整管线**:`eval_upstream` 对文本段调
  `scorer.rank_detailed`(= collect + merge)—— stage2 内部再跑一遍
  stage2+stage3,注释自己说"文本段走统一打分"。层级可接受(递归折叠
  语义如此)但**必须显式**:入口收敛为一个函数,禁止散落。

### Stage3 后处理

- **P11-9 merge 住在 family 模块**:stage3 第一步(×priority / 全局排序 /
  跨家族去重)实现在 `family/mod.rs` 的 `UnifiedScorer::merge`,而
  postprocess 在 `fsm/post.rs` —— 同一阶段跨两个顶层模块。
- **P11-10 promote_single_letter 自成一档**:它是**位置规则**(置顶),
  却夹在 merge 与 FilterChain 之间,不进 Verdict 语义;`candidates_detailed`
  镜像路径靠"共用此函数"的约定保持一致(post.rs:441 注释自认)。
- **P11-11 造词单字区嵌套废块**:post.rs:294-346 双层 `{ { … } }` 空作用域,
  可读性差。
- **P11-12 视图组装混格式化**:调试 meta 字符串(`[score family/source]`)
  在 fill_view 里格式化 —— 展示语义混进视图组装(轻微,顺带归位)。

### 跨阶段

- **P11-13 候选双路径**:`engine.rs candidates_detailed()` 对同一输入重跑
  `rank_detailed`(+ promote_single_letter)做"镜像" —— 面板 panel.meta
  已是同源数据,双路径一致性靠约定不靠结构。W2(meta 对齐)的回归
  正源于此。

## 二、目标结构

```
fsm/
├── key.rs        键枚举 / StateFlags / as_command_char(不动)
├── pre.rs        Stage1:ControlStage —— 键路由决策 + stage2 门面调用
├── family.rs      Stage2:FamilyPipeline 聚合体 + 三态入口 + 拼音组合
├── magic.rs       Stage2:MagicSession(链式命令)全会话逻辑(新文件)
├── chain.rs       链式段解析(不动)
├── post.rs        Stage3:合成/置顶/过滤/造词区/视图组装 + FilterChain
└── state.rs       StateMachine 壳(或并入 pre.rs)—— R1 后定去留
```

边界规则(写进 fsm/mod.rs,作为本轮的"宪法"):

1. **Stage1 只问两件事**:这枚键归输入法还是应用;归输入法时调用 stage2
   的**门面方法**,不触碰聚合体内部字段(panel/comp 对 stage1 不可见)。
2. **Stage2 拥有全部会话状态**(Composition / CandidatePanel / MagicSession),
   对 stage1 只暴露:step_key / step_char / select / select_page_digit /
   move_highlight / change_page / nudge_cursor / reset / make_view / view。
3. **Stage3 是纯函数管道**:输入 = 家族收集结果 + 会话只读快照;输出 =
   PanelItem 序列。合成(merge)、置顶(promote)、过滤(chain)、造词区、
   视图组装按序排布,全部住在 post.rs(merge 函数本体迁入或 re-export)。
4. **视图 helper 单点**:commit_view 系 / escape_preedit 集中一处,禁止
   内联重写。

## 三、工作项(四步,每步独立可验证:全测试 + 评测持平)

### R1 — Stage1 收敛(P11-1/2/3/4)
- `StateMachine::step` 与 `ControlStage::route_key` 归一化合一:step 保留
  唯一一份(或者反过来,route_key 保留),另一份只转调。
- stage2 新增门面方法:`select_page_digit(n)`(页内序号→全局序→select)、
  `nudge_cursor(±1)`(光标 clamp 内聚 Composition);pre.rs 改调门面。
- `state_flags()` / `change_page()` impl 块从 state.rs 迁到 family.rs。
- 路由矩阵注释随实现搬到 pre.rs 模块头。

### R2 — Stage2 拆文件(P11-5/6/7)
- 新建 `fsm/magic.rs`:MagicSession 结构体 + query_magic /
  query_chained_magic / eval_upstream / eval_command / ensure_command /
  select_magic / force_fire / rebuild_magic_view / sync_magic_preedit 全量
  迁出(跨文件 impl FamilyPipeline,同 post.rs 现行模式)。
- 新建 `fsm/views.rs`(或并入 post.rs 尾部):commit_view / commit_view_at /
  passthrough_view / delete_view / escape_preedit 集中。
- `commit_and_reset(&mut self, text, family, env)` 统一出口,六处调用点
  收敛(family 参数与大小写回填语义在函数内表达)。
- `Composition::sync_preedit()` 消八处重复。

### R3 — Stage3 归位(P11-9/10/11/12)
- `UnifiedScorer::merge` 迁到 post.rs(family/mod.rs 留 re-export 或调用点
  改路径);UnifiedScorer 回归纯 stage2(collect)角色。
- `promote_single_letter` 显式命名为 stage3 置顶段(改名
  `promote_rules` / 文档标注"位置规则,先于过滤链"),或改造成 FilterChain
  内置 filter —— 以**不改行为**为准绳,倾向前者。
- 拍平造词单字区双层 block;调试 meta 格式化移到 fill_view 顶部小函数。

### R4 — 单一候选路径(P11-13)
- `candidates_detailed()` 改读 `pipeline.panel.meta` 映射(不再重跑
  rank_detailed);W2 的 meta 对齐由结构保证。
- `engine.predict`/`key` 侧核对 view 与 detailed 的时序(重排后同帧)。

## 四、验收

1. `cargo test -p ime-core -p swift-ime` 全绿(失效测试按惯例删除)。
2. 评测持平:tc_dict_sample 98.0±0.2、tc_en_sample 99.4。
3. fsm/ 各文件行数均衡(目标:无 >800 行文件;family.rs 预计降至 ~700)。
4. fsm/mod.rs 的边界规则四条与代码实际一致(以代码为准复核一遍)。

## 五、执行记录(2026-09-05,四步全部落地)

- **R1 ✓**:action 归一化收口 `StateMachine::step`(route_key 只返回裸视图);
  stage1 改调门面 `select_page_digit` / `nudge_cursor`(不再触碰
  panel/comp 内部);`state_flags`/`change_page` impl 迁 family.rs;
  路由矩阵权威版随实现搬到 pre.rs 模块头。
- **R2 ✓**:新建 `fsm/magic.rs`(439 行,MagicSession + 链式命令全逻辑);
  view helpers(commit_view 系 + escape_preedit)集中 post.rs;
  `commit_raw_and_reset` 统一出口(六处收敛);`Composition::sync_preedit`
  (八处收敛)。中途发现并修复:块迁移时误删 `apply_input_casing`,
  已恢复(测试 `prefix_case_applied_to_completion_suffix` 兜住)。
- **R3 ✓**:`UnifiedScorer::merge` 迁入 post.rs(family/mod.rs 回归纯
  stage2 collect);promote_single_letter 文档标注"置顶段,先于过滤链";
  造词单字区双层废块拍平。
- **R4 ✓(无代码改动)**:`candidates_detailed()` 早在 S3 轮已改为读
  面板(`pipeline.detailed()`),勘察时误判双路径仍在 —— 目标已达成,
  仅清理认识。StepEnv 按 stage 拆最小接面按预案跳过(收益 < 风险)。

**验收结果**:全测试绿(149+21+7+11+14+2);评测持平(98.0 / 99.4);
fsm/ 行数 family.rs 1048(略超 800 目标 —— 剩余为拼音组合核心 + 300 行
测试,进一步拆分收益低);deb 构建通过。

### 复核补记(2026-09-06,提交后二次核对)

- **P11-7 漏网一处**:`magic.rs` 链式上游回退仍是手写 committed+raw 拼接,
  改调 `sync_preedit()`(其余 preedit 手写点均为不同语义:大小写回填 /
  候选预览,保留)。
- **clippy 清零**:ime-core 10 条 + swift-ime 1 条全修(路由矩阵收敛后
  `route_key` 的 `table` 参数与 `StateMachine` 导入已无用途,删除;
  sort_by→sort_by_key×2;engine 构造器 let-binding;EnglishFamily 补
  Default;emoji.rs 文档注释制表符;config.rs FamilyTopNConfig 改派生
  Default)。
- 复验:测试全绿、评测持平(98.0 / 99.4)、双 crate clippy 0 警告。
