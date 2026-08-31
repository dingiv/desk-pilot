# swift-ime 第九轮 — FSM 文件结构重排:三阶段三文件,显式命名

> 创建: 2026-08-31。round8 完成三阶段语义拆分,但**文件结构没跟上**:
> stage1 的代码住在 `router.rs`(名字却叫 router),`state.rs` 里的
> `StateMachine` 身兼 stage2/stage3/状态聚合三职,`StateMachineTable`/
> `route` 命名与实际职责脱节。本轮:文件与结构体按三阶段显式对齐。

## 一、用户指定的目标结构

| 文件 | 职责 | 结构体 |
|---|---|---|
| `fsm/pre.rs`(新增) | **Stage 1 系统控制**:键的系统判定与流转(提交/选词/翻页/高亮/透传/命令触发),显式结构体承载 | stage1 显式结构体 |
| `fsm/state.rs`(原 router.rs 改名) | **顶层状态机**:`StateMachineTable` → `StateMachine`,`.route` → `.step`;StateFlags/KeyEvent/KeyKind/ComposeState 随迁 | `struct StateMachine` |
| `fsm/family.rs`(原 state.rs 改名) | **Stage 2 家族分发**:把 `crate::family` 的能力展开、分发给管线;聚合体(comp/panel/magic)随迁 | `struct StateMachine` → `struct FamilyPipeline` |
| `fsm/post.rs`(新增) | **Stage 3 后处理**:merge 合成 → promote → 造词单字区 → PanelItem 落位 → 视图组装(fill_view/rebuild_magic_view) | post 管线函数/结构体 |

**改名对**:
- `struct StateMachineTable` → `struct StateMachine`
- `struct StateMachine`(现,状态聚合)→ `struct FamilyPipeline`
- `StateMachineTable::route` → `StateMachine::step`

**依赖方向**(用户点名,必须单向):
`fsm/*` → `crate::family/*` ✓;**family 不得反向依赖 fsm**。
现状违规:S5 在 `family/magic/mod.rs` 里 `use crate::fsm::state::StepEnv`
(query 签名收 `&dyn StepEnv`),`MagicMember::predict` 同样 —— 本轮解开。

## 二、现状 → 目标映射(逐项搬移清单)

### router.rs(→ state.rs)
| 现状 | 去向 |
|---|---|
| `StateMachineTable`(route/route_inner/flags) | `state.rs::StateMachine`(step = stage1 入口) |
| route_inner 的键分派 match(Space/Enter/Backspace/Digit/Page±/±/=/Escape/CtrlAlt/Snippet command_char) | `pre.rs`(stage1 显式结构体/函数) |
| StateFlags / state_flags() | state.rs(随 Table) |
| KeyEvent / KeyKind / command_char | pre.rs(KeyEvent 若被 engine 对外暴露,类型定义移 fsm/mod.rs 再 re-export) |
| `StateMachine`(面板方法:change_page/move_highlight) | family.rs(随 FamilyPipeline —— 面板行为是 stage2/3 语义) |

### state.rs(→ family.rs + post.rs)
| 现状 | 去向 |
|---|---|
| `struct StateMachine`(comp/panel/magic/context/last_commit_family) | `family.rs::FamilyPipeline` |
| ComposeState | state.rs(顶层枚举,pre/family 共用) |
| step/handle_idle/handle_pinyin/handle_snippet(家族入口分派) | family.rs(stage2) |
| query_pinyin/query_magic/query_chained_magic/eval_upstream | family.rs(stage2;query_magic 已是家族 query 的薄壳) |
| **postprocess**(merge→promote→造词重排)/ PanelItem / CandMeta | post.rs(stage3) |
| **make_view / fill_view / rebuild_magic_view**(视图组装) | post.rs(stage3 产出) |
| select / select_magic / pinyin_terminator / pinyin_backspace(提交/编辑动作) | family.rs(系统动作入口在 pre,实现落 family) |
| StepEnv trait | family.rs(见依赖单向化) |
| CandidatePanel / Composition / MagicSession 聚合体 | family.rs(跟 FamilyPipeline;如过大可再拆 aggregates.rs,本轮不做) |
| promote_single_letter / apply_input_casing | post.rs(promote)/ family.rs(casing,提交回填语义) |

### 依赖单向化(本轮硬性目标)
- 现状违规:`family/magic/mod.rs` 的 `query`/`MagicMember::predict*` 收
  `&dyn StepEnv`(fsm 侧 trait)——family → fsm 反向。
- 方案:**family 定义窄接口 `FamilyEnv`**(只含家族需要的能力:
  record_pick / learn_phrase / learn_composed_phrase / compose_single_chars /
  expander / voice_cmd_tx …),`MagicMember::predict*` 与 `query` 改收
  `&dyn FamilyEnv`;fsm 侧 `StepEnv: FamilyEnv` 超集继承,Dispatcher(现
  ImeEngine)一处实现两用。engine/step_env_tests 的 impl 相应标注。
- 结果:`family/*` 只 `use crate::family::*` + `crate::store`,零 fsm 引用。

## 三、实施步骤(每步独立提交,golden 全程护航)

| 步 | 内容 | 规模 | 风险 |
|---|---|---|---|
| R1 | **纯改名**:`state.rs → family.rs`(StateMachine→FamilyPipeline)、`router.rs → state.rs`(StateMachineTable→StateMachine、route→step);engine.key_ctx 调用点同步;`mod` 声明更新 | 机械(全文件改名 + 类型名替换) | 低 |
| R2 | **post.rs 抽取**:postprocess/make_view/fill_view/rebuild_magic_view/PanelItem/CandMeta 迁出 family.rs | 中 | 低 |
| R3 | **pre.rs 抽取**:route_inner 的键分派迁入(pre 的显式结构体);StateMachine.step 调 pre;KeyEvent/KeyKind 归位 | 中 | 中(分派上下文多字段) |
| R4 | **依赖单向化**:FamilyEnv 窄接口(family 侧定义),MagicMember::predict*/query 改签名,StepEnv 成为 fsm 侧超集 | 中 | 中(接口迁移) |

每步后:ime-core + swift-ime 全量测试;golden 12 项断言不变。

## 四、目标形态(完成后)

```
fsm/
├─ mod.rs      模块声明(StepEnv 重导出等)
├─ state.rs    struct StateMachine(原 Table)+ StateFlags + ComposeState
│              .step(key) = stage1 入口
├─ pre.rs      stage1:系统控制(显式结构体 + 键分派)
├─ family.rs   struct FamilyPipeline(原 StateMachine):stage2 家族分发
│              + StepEnv trait + 三聚合体(comp/panel/magic)
└─ post.rs     stage3:合成/调整/造词重排/PanelItem/视图组装

engine.rs      key_ctx → StateMachine.step(改名后)
family/        零 fsm 引用(FamilyEnv 窄接口反向解耦)
```

调用链示意:
```
engine.key_ctx(key)
  └─ StateMachine.step(key, &dyn StepEnv)          # state.rs:stage1
       ├─ 系统键 → pre.rs 处理(提交/翻页/透传)→ view
       └─ 字符键 → FamilyPipeline.char(ch)          # family.rs:stage2
            ├─ 家族展开/分发(pinyin/english/magic query)
            └─ post.rs::finish(collected, &pipeline) # stage3
                 └─ ImeView
```

## 五、不变式

- 行为零变化:golden 12 项 + 全量测试每步全绿(本轮纯结构,无语义改动)
- R4 的 FamilyEnv 迁移对家族行为零影响(接口收窄,能力集合不变)
- 外部 API(engine 的 pub 方法)签名不变(key_ctx 内部实现换 step)

## 关联

- 上一轮:issues-round8.md(三阶段语义拆分 —— 本轮把文件结构对齐语义)
- 用户原话摘要:"stage1 放在 router.rs 就该显式把结构体写出来;
  router.rs 改成 state.rs,新增 pre.rs 处理第一阶段,state.rs →
  family.rs 处理第二阶段家族分发,新增 post.rs 处理后处理;
  StateMachineTable → StateMachine,StateMachine → FamilyPipeline,
  route → step;fsm 依赖 family 保持单向,fsm 里的 family 负责展开分发"

## 执行记录

- **R1 ✅ 纯改名落地**:
  - 文件:`router.rs → state.rs`、`state.rs → family.rs`(git mv 保历史)
  - 类型:`StateMachineTable → StateMachine`;原 `StateMachine → FamilyPipeline`
  - 方法:`route/route_inner → step/step_inner`;PerContext 字段
    `sm → pipeline`(table 字段名保持)
  - 引用面:engine.rs / fsm::{state,family} / lib.rs 导出
    (`pub use fsm::state::{KeyEvent, KeyKind, StateFlags, StateMachine}`;
    顶层 router/state 别名删除 —— apps 用全路径,且顶层 family 名已被
    家族模块占用,fsm::family 不做顶层别名)/ apps swift-ime(tui/fcitx5
    的 fsm::router:: → fsm::state::)/ examples / tests 全量同步
  - 过程修正:全局替换的顺序坑(Table→StateMachine 又被
    StateMachine→FamilyPipeline 吞掉;state.rs 内 Table 的 impl 误改)
    逐一回改;PerContext.table 类型、sync_from/step 参数类型、
    FamilyPipeline 关联函数(passthrough_view 等)前缀对齐
  - 测试:ime-core 157+21+2、swift-ime 7+12+15 全绿

- **R3 ✅ pre.rs 抽取**(先于 R1/R2 记录顺序无碍):
  - **`ControlStage` 显式结构体**(零大小,Copy):stage1 是管线的一员,
    不是散落的 match —— StateMachine 持 `control: ControlStage` 字段
  - `ControlStage::route_key(table, pipeline, key, env)`:系统键就地处理
    (提交/选词/翻页/高亮/透传/命令文本 hoist),字符键交 stage2;
    action NONE→HANDLED 不变式、flags 同步随迁
  - handled_empty_view/command_char 随迁;KeyEvent/KeyKind 仍定义于
    state.rs(stage1 的输入类型)
- **R2 ✅ post.rs 抽取**:PanelItem/CandMeta(产出结构)+ postprocess
  (merge→promote→造词重排)+ make_view/fill_view(滑动窗口)/
  rebuild_magic_view 自 family.rs 迁出;FamilyPipeline 的 stage3 行为
  分片(同 crate 跨文件 impl);escape_preedit/pending_full_comp_count
  可见性放宽
- **R1 ✅ 纯改名**:
  - 文件:router.rs → state.rs、state.rs → family.rs(git mv 保历史)
  - 类型:StateMachineTable → StateMachine;原 StateMachine → FamilyPipeline
  - 方法:route/route_inner → step/step_inner;PerContext 字段 sm → pipeline
  - 引用面:engine / lib.rs 导出 / apps(tui/fcitx5)/ examples / tests
    全量同步;顶层 router/state 别名删除(顶层 family 名被家族模块占用)
  - 过程修正:全局替换顺序坑(Table→StateMachine 又被吞)与
    PerContext.table 类型/参数类型/关联函数前缀逐一回改(用户点名先做,提前于 R1-R3):
  - **`FamilyEnv` trait 定义在 family/mod.rs**(family 需要什么,family
    自己说了算):expander / voice_cmd_tx(默认 None)/ record_pick /
    learn_phrase / learn_composed_phrase / compose_single_chars
  - `MagicMember::predict*/query` 的 `&dyn StepEnv` → `&dyn FamilyEnv`;
    `pick/tick` 的 `&mut StateMachine` 参数化(成员只用 sm.ctx 与
    sm.comp.buffer → 参数 `ctx: usize, buffer: &str`)
  - fsm 侧:`StepEnv: FamilyEnv` 超集,只剩 fsm 特有能力(scorer /
    first_syllable / magic);ImeEngine 与 TestEnv 各自实现 FamilyEnv
    (家族能力)+ StepEnv(fsm 能力)
  - **验证**:`grep crate::fsm family/` 零命中(注释链接一并清理)——
    family → fsm 反向依赖清零,fsm → family 单向成立
  - 测试:ime-core 157+21+2、swift-ime 7+12+15 全绿
