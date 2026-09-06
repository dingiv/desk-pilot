# swift-ime 第十二轮 — 边界规范化 + stage2/stage3 交付件解耦

> 创建: 2026-09-06。议题演进:①前端 FFI 越界评估 → ②事件驱动模型 →
> ③engine.rs TODO 处置 → ④**stage2/stage3 解耦(本轮主体,计划见 §六)**。
> §一~§五为已落地记录,§六为待执行计划。

## 一、评估结论(以代码为准)

前端(fcitx5 FFI / TUI / ibus / imk / tsf)只调 `ImeEngine` 公共 API,
**前端本身不越界**。越界发生在 engine 壳内部 —— 壳绕过 stage1/stage2
门面直接读写聚合体字段。逐个公共出口核定:

| 出口 | 判定 | 依据 |
|---|---|---|
| `key_ctx` → `table.step` | ✅ 正统 | stage1 统一键入口 |
| `select_ctx` → `pipeline.select` + `sync_from` | ✅ 正统 | `select` 是边界规则2明列门面;编程式选择(鼠标/序号)非键事件,stage1 无路由可做 |
| `reset_ctx` → `pipeline.reset` | ✅ 正统 | 门面方法 |
| `buffer`/`candidates`/`last_meta` | ✅ 可留 | 规则3"只读快照"允许;测试/调试面 |
| `commit_pending_ctx` | ❌ 越界 → **已修** | 壳内复刻 apply_input_casing + raw_buffer 兜底提交语义(P11-6 收敛点漏网) |
| `view()` | ❌ 越界 → **已修** | 壳内手工拼 ImeView,与 post.rs `fill_view` 平行重复(meta 格式化第二份);且忽略翻页窗口,已与显示分叉 |
| `candidates_detailed` | ❌ 越界 → **已修** | 壳内读 `state`/`panel.fresh`/`magic.active` 做 Snippet 会话分支 —— 会话语义漏进壳 |
| 配置 setter(set_page_size/enabled/top_n/…) | ✅ 可留 | engine 是装配 owner |

## 二、壳内越界收口(已落地)

- `magic_tick_ctx` 编排下沉 `FamilyPipeline::magic_tick(disp)`;壳只留
  flags 同步与观测日志。
- `is_voice_ctx_alive` → 门面 `asr_probe() -> (bool, String)`。
- `remove_ctx` 成员释放 → 复用 `clear_active_command()`。
- `set_page_size` → 门面 `FamilyPipeline::set_page_size`。
- `FamilyPipeline::pending_commit_text()`:提交语义单点(大小写回填 /
  raw_buffer 兜底),`commit_pending_ctx` 改调。
- `FamilyPipeline::snapshot_view()`:与 `make_view` 同源 `fill_view`,
  `view()` 改调 —— 壳内手工拼装 ImeView 禁止。副作用(行为修正):
  view() 修掉了忽略翻页窗口的隐性分叉。
- Snippet 会话分支下沉 `FamilyPipeline::detailed()`。

## 三、事件驱动模型(已落地)

- 新建 `fsm/event.rs`:`ImeEvent` 三分类 —— `Key`(路由矩阵)、
  `Control`(编程式会话操作,必被消费,直达 stage2 门面)、
  `Async`(命令会话 tick,无推进返回 None)。
- `StateMachine::handle_event()` 统一事件入口;`step` 退化为 `Key`
  事件薄包装;壳 `event_ctx(ctx, event)` 唯一入口,
  `key_ctx`/`select_ctx`/`reset_ctx`/`magic_tick_ctx` 全改为事件构造
  薄包装;删除 `StateMachine.control` 无用字段。

边界规则第 5 条:**壳与前端的一切动作 = 事件;事件即 stage1 的唯一入口**。

## 四、engine.rs 四个 TODO 的处置(已落地)

| TODO | 分析 | 处置 |
|---|---|---|
| `expander` 引擎要持有吗? | **要**。engine 即 `FamilyEnv` 实现体,snippet 展开经 `env.expander()` | 保留,TODO 转结论性文档 |
| `provider` 字段 | **冗余**,与 expander 同持一个 Arc | 删除;`Expander::set_variable` 单点写入 |
| `voice_state` 泛化 async state? | 引擎只在装配期交接;读方各经渠道拿 Arc;泛化注册表 YAGNI | 删除字段;accessor 从 `magic.resources()` 槽取 |
| 状态机概念不可见 / 最小知道原则 | **成立**。`PerContext` 裸露聚合体字段 | 重构 `Session` 封装:`handle(event)` + 门面/只读转发;字段访问权由 Session 独占 |

## 五、命名对齐(已落地)

`ImeEngine.contexts` → `sessions: Mutex<HashMap<usize, Session>>`。

---

## 六、状态归属重构 + stage2/stage3 交付件解耦(本轮主体计划,待执行)

> 6.x 版:v2 —— 吸收 state.rs / family.rs 三条 review 意见(FIXME:
> 状态机里没有状态 / pipeline 改纯函数 / 会话数据与家族数据分离)。

### 6.0 现状耦合(post.rs 里的 `impl FamilyPipeline`,35 处字段触碰)

| post.rs 现有成员 | 实际归属 | 耦合方式 |
|---|---|---|
| `postprocess(&mut self, …)` | stage3 核心 | 收 `collected`,却 `&mut self`:读 `comp.buffer/context/state`,写 `pending_full_comp_count` |
| `fill_view`/`make_view`/`snapshot_view` | 视图渲染 | 直读 `panel/comp/state/candidate_meta_enabled` |
| `pending_commit_text` | 提交语义 | 直读 `panel.items` + `comp.raw_buffer` |
| `rebuild_magic_view(&mut self)` | **stage2 会话逻辑** | 误居 post.rs |
| `FilterChain`/`merge`/`promote_single_letter` | stage3 自有 | 无耦合 ✓ |

另有两处结构性异味(review 意见的靶心):

- `StateMachine` 只有 `flags` 镜像,**没有状态** —— 真正的会话状态
  (comp/panel/context/magic/state)全在它旁边的 `FamilyPipeline` 里,
  "状态机"名不副实。
- `FamilyPipeline` 既装状态又装逻辑,且 `pending_full_comp_count`
  这种 stage3 中间值也挂在持久状态上(带出槽 hack)。

### 6.1 架构原则(review 意见 + 前议,设计前提)

1. **两路径 stage1 判定**:魔法命令家族走完全不同的道路(会话自管
   面板,不经 stage3);其余家族参与打分,走 stage3。路径归属在
   stage1 路由时判定,不由 stage2 内部隐式分叉。
2. **stage2 不调用 stage3**:stage2 产出交付件传递给 stage3;stage3
   处理完返回外层路由模块。编排权在 stage1。
3. **状态机要有状态**(state.rs FIXME):`StateMachine` 是会话状态的
   唯一居所 —— 输入上下文、组合文本、面板(分页/高亮/候选)、视图
   状态全在状态机里;flags 从"唯一内容"降为派生镜像。
4. **stage2 纯函数化**(family.rs FIXME):`FamilyPipeline` 撤销,
   逻辑改写为以 `&mut StateMachine` 为状态的**转移函数**
   `fn(state, event, env) -> Stage2Result`;状态由上层状态机持有,
   三个阶段的流水线都能访问同一份状态。
5. **会话数据 vs 家族数据分离**(family.rs FIXME):
   - **会话级**(每个 session 一份,进 `StateMachine`):`state`、
     `comp`(Composition)、`panel`(CandidatePanel)、
     `magic.active` 成员**实例**/predictions/hints、`context`
     (短期提交上下文)、`ctx` 挂号、`flags`。
   - **家族级**(全局单份,引擎持有,**已就位**,不下沉):scorer、
     三家族 Arc、MagicFamily 注册表 + resources(voice 槽/req 配置/
     snippets/expander)、io/frontend 句柄。
   - 判据:成员实例在会话(一个 ctx 的 #asr 会话独立),实例引用的
     资源在家族(Arc 共享)。
   - `candidate_meta_enabled` 是配置不是会话状态 → 移引擎,
     经 `PanelSnapshot` 带给渲染;`pending_full_comp_count` 带出槽 →
     由 `PostOutcome.full_comp_count` 交付件取代。

### 6.2 目标结构

```
fsm/state.rs    StateMachine = 会话状态机(状态唯一居所):
                state / comp / panel / magic_session / context / ctx / flags
fsm/pre.rs      stage1:路由矩阵 + 路径判定(魔法 vs 打分)+ stage2↔stage3 编排
fsm/family.rs   stage2 纯转移函数:step/select/change_page/…(&mut StateMachine, env)
fsm/magic.rs    stage2 魔法路径纯转移函数(会话自管面板,不经 stage3)
fsm/post.rs     stage3 纯函数管道:postprocess / render / pending_commit_text
fsm/event.rs    ImeEvent 三分类(已落地,不变)
```

数据流(打分路径):

```
stage1: Key → 路由矩阵 → 打分路径
  → stage2: family::step(&mut sm, key, env) → Stage2Result::NeedsPost(PostRequest)
  → stage3: postprocess(&req, env) → PostOutcome
  → stage2: family::apply_post_outcome(&mut sm, outcome)(落位,唯一写口)
  → render(&sm.snapshot()) → ImeView → 返回外层路由模块 → 前端
```

### 6.3 交付件契约(stage3 拥有定义,两阶段唯一交互面)

```rust
pub struct PostRequest {       // stage2 → stage3(只读快照 + 收集结果)
    pub buffer: String,
    pub context: String,
    pub state: ComposeState,
    pub collected: Vec<FamilyCandidates>,
}
pub struct PostOutcome {       // stage3 → stage2(经 stage1 转交)
    pub items: Vec<PanelItem>,
    pub full_comp_count: usize,        // 取代 pending_full_comp_count 带出槽
}
pub struct PanelSnapshot {     // stage2 → 渲染(只读)
    pub items: Vec<PanelItem>, pub highlight: usize,
    pub page: usize, pub page_size: usize,
    pub preedit: String, pub cursor: usize,
    pub state: ComposeState, pub candidate_meta: bool,
}
pub(crate) enum Stage2Result { // stage2 对 stage1 的返回:两路径显式化
    Done(ImeView),            // 魔法路径/无候选/提交/透传:面板已就绪
    NeedsPost(PostRequest),   // 打分路径:交付件待 stage1 转 stage3
}
```

### 6.4 stage3 纯函数管道(post.rs,不再有 impl StateMachine)

```rust
pub fn postprocess(req: &PostRequest, env: &dyn StepEnv) -> PostOutcome;
pub fn render(snap: &PanelSnapshot) -> ImeView;          // fill_view 降级
pub fn pending_commit_text(snap: &PanelSnapshot) -> String;
```

### 6.5 工作项(每步独立验证:全测试 + 评测持平 98.0/99.4)

- **S1 状态上移**:`StateMachine` 吸收全部会话状态(state/comp/panel/
  magic_session/context/ctx);家族级数据核查清单核对(不下沉);
  `candidate_meta_enabled` 移引擎;`pending_full_comp_count` 待 S3 由
  `PostOutcome` 取代。engine `Session` 收缩为 `StateMachine` 单字段
  (或直接别名)。
- **S2 stage2 纯函数化**:family.rs/magic.rs 的 `impl FamilyPipeline`
  改写为 `impl StateMachine` 转移函数(签名 `( &mut self, …)` 语义
  不变,状态已在自己身上);pre.rs 路由改调新签名;stage2 打分路径
  返回 `Stage2Result::NeedsPost`。
- **S3 stage1 编排 + stage3 纯函数化**:pre.rs 按两态编排接入
  `postprocess(&PostRequest)`(自由函数);`apply_post_outcome` 落位;
  `rebuild_magic_view` 迁 magic.rs;`pending_full_comp_count` 删除。
- **S4 渲染纯函数化**:`fill_view` 系 → `render(&PanelSnapshot)` +
  stage2 薄包装(对外出口签名不变);`pending_commit_text` 只吃快照。
- **S5 宪法成文**:fsm/mod.rs —— 第 1 条补"路径判定是 stage1 职责";
  第 2 条改"会话状态唯一居所是 StateMachine";新增第 6 条"stage2↔
  stage3 只经交付件交互,stage3 禁止持有状态机引用,编排权在 stage1;
  家族级数据永不入会话状态机"。

### 6.6 验收

1. `cargo test -p ime-core -p swift-ime` 全绿(失效测试按惯例删除)。
2. 评测持平:tc_dict_sample 98.0±0.2、tc_en_sample 99.4。
3. `StateMachine` 持有全部会话状态;代码库中不再存在 `FamilyPipeline`
   类型;post.rs 内无 `impl StateMachine`(只剩 stage3 自有类型与自由
   函数);`rebuild_magic_view` 在 magic.rs。
4. fsm/mod.rs 边界规则与代码实际一致(以代码为准复核)。

### 6.7 执行记录(2026-09-06,S1–S5 全部落地)

- **S1 ✓ 状态上移**:`FamilyPipeline` 撤销(类型全库 0 残留),全部
  会话字段移入 `StateMachine`;engine `Session` 收缩为 `{ sm }`(响应
  行内 FIXME:状态进状态机、table→sm)。
- **S2/S3 ✓ 交付件解耦**:`PostRequest`/`PostOutcome` 契约落地;
  `postprocess` 改 post.rs 自由函数(不持状态机引用);
  `pending_full_comp_count` 带出槽删除(由 `PostOutcome.full_comp_count`
  取代);`StateMachine::resolve` 为路由层编排点(stage2 经
  `query_pinyin = collect_pinyin + resolve` 组合,不直调 stage3);
  `rebuild_magic_view` 迁回 magic.rs。执行简化:计划的 `Stage2Result`
  两态枚举收敛为 `Option<PostRequest>`(Done 变体无构造点 —— 魔法路径
  直接返回视图)。
- **S4 ✓ 渲染纯函数化**:`PanelSnapshot` + `render`/`pending_commit_text`
  /`escape_preedit`/视图 helper(commit_view 系)全部降为自由函数;
  post.rs 内 `impl StateMachine` 归零(stage2 薄包装
  make_view/snapshot_view/panel_snapshot 迁 family.rs)。
- **S5 ✓ 宪法成文**:fsm/mod.rs 六条边界规则重写(编排权在 stage1、
  会话状态唯一居所是 StateMachine、家族级数据不入状态机、stage3 禁持
  状态机引用、视图 helper 单点、壳=事件唯一入口)。
- 执行偏差:计划的 `candidate_meta_enabled` 移引擎未做 —— 它是会话级
  视图配置,移出反而要穿 env 传递;保留在状态机并经 `PanelSnapshot`
  带给渲染(语义达标,机制偏差已记录)。

**验收结果**:全测试绿(149+21+7+11+14+2);clippy 双 crate 0 警告;
评测持平 tc_dict 98.0 / tc_en 99.4;代码库中 `FamilyPipeline` 类型
0 残留;post.rs 内 `impl StateMachine` 0 残留。
