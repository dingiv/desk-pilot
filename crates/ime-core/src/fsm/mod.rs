//! fsm — 输入组合状态机层,文件按三阶段组织(round9;round11 规范化)。
//!
//! ## 三阶段边界规则(round12 宪法)
//!
//! 1. **Stage1(`pre`)是路由层 + 编排者**:①判定键归属(输入法/应用);
//!    ②判定路径归属(魔法命令 vs 打分家族);③编排 stage2↔stage3 ——
//!    把 stage2 产出的交付件([`post::PostRequest`])传给 stage3 纯函数,
//!    回执([`post::PostOutcome`])交回 stage2 落位。不触碰状态机字段。
//! 2. **会话状态的唯一居所是 `ControlPane`([`control`])**:state/comp/
//!    panel/magic 实例/context/ctx 全在状态机身上;flags 只是派生镜像。
//!    stage2(`family`+`magic`)是它身上的**转移函数**(`&mut self`,
//!    纯转移、无自有状态),是唯一的状态写入口。
//! 3. **家族级数据永不入会话状态机**:scorer / 家族 Arc / MagicFamily
//!    注册表与 resources 全局单份,由引擎持有(判据:成员实例在会话,
//!    实例引用的资源在家族,经 Arc 共享)。
//! 4. **Stage3(`post`)是纯函数管道,禁止持有状态机引用**:只吃交付件
//!    —— `postprocess(PostRequest) -> PostOutcome`、`render(&PanelSnapshot)`
//!    、`pending_commit_text(&PanelSnapshot)`。合成(merge)→ 置顶 →
//!    过滤链 → 造词单字区 → 视图组装,按序全部住在 post.rs。
//! 5. **视图 helper 单点**(commit_view 系 / escape_preedit / render):
//!    集中定义,禁止内联重写;壳禁止手工拼装 ImeView。
//! 6. **engine 壳同受规则 2 约束**:会话状态读写只经 `Session` 的门面/
//!    只读转发,壳内禁止出现状态机字段访问(装配配置 setter 除外);
//!    一切动作 = 事件(`ImeEvent`),事件即 stage1 的唯一入口。
//!
//! ## 文件
//!
//! - [`control`] — `ControlPane`:**会话状态机**(状态唯一居所:state/
//!   comp/panel/magic/context/ctx;flags 派生镜像)。`handle_event` 统一
//!   事件入口(action 归一化 + flags 镜像收口);`resolve` 为路由层编排点
//!   (交付件 → stage3 → 落位)
//! - [`pre`] — Stage1 系统控制:键路由决策矩阵(权威版)随实现
//! - [`family_prediction`] — stage2 打分家族路径的转移函数(收集 → 产出 PostRequest;
//!   接 PostOutcome 落位)+ Composition/CandidatePanel/StepEnv 定义
//! - [`magic_flow`] — stage2 的魔法命令会话分片(MagicSession 全逻辑)
//! - [`chain`] — `'` 链式输入的段解析(纯函数):`ti'an` / `X'#cmd` /
//!   `X''#cmd` 的语法在此,状态机与拼音家族按它路由
//! - [`key`] — 键枚举 / 状态标志位 / 命令字符 hoist
//! - [`event`] — 统一事件模型(round12):键盘 / 控制 / 异步三类事件,
//!   一律从 stage1 进,壳与前端不得绕过自行调用 stage2
//!
//! - [`post`] — **stage3 后处理纯函数管道**:交付件契约(PostRequest/
//!   PostOutcome/PanelSnapshot)+ postprocess/render/pending_commit_text
//!   自由函数;合成 → 置顶 → 过滤链 → 造词单字区 → 视图组装
pub mod chain;
pub mod event;
pub mod family_prediction;
pub mod key;
pub mod magic_flow;
pub mod post;
pub mod pre;
pub mod control;
