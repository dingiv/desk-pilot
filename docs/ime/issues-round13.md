# swift-ime 第十三轮 — ControlPane 三大事件分发 + 双路键处理

> 创建: 2026-09-06。用户模型:"状态机 → ControlPane 系统控制,分发三大
> 事件类型;双路键处理(第一路 FamilyPrediction / 第二路 MagicFlow);
> 封装 ImeView 返回"。当轮落地。

## 一、目标结构(用户口径 → 代码)

```
ControlPane(系统控制,原 StateMachine)
 ├── 分发三大事件类型:
 │   ① 普通键           → 第一路 FamilyPrediction
 │   ② `#` 触发键        → 第二路 MagicFlow 同步数据流
 │      异步信号(IoThread 注入 MagicTick)→ 第二路 MagicFlow 异步数据流
 │   ③ 系统控制事件      → 提前拦截并返回(提交/选词/翻页/复位),不进双路
 └── 处理完成统一封装 ImeView 返回(handle_event:action 归一化 + flags 镜像)

第一路 FamilyPrediction(family.rs)
   不依赖 ControlPane —— 只吃 SessionState(会话纯数据);
   三个家族对象(pinyin/english/emoji Arc)自己有状态、跨会话共享,由引擎持有;
   → 后处理(post.rs 纯函数):家族间综合评分(merge)→ 评分微调(置顶+过滤链)
     → 造词单字区;输入统计(提交学习 record_pick/L0)与
     上下文构建(InputContext 经 scorer.collect 在预测前注入各家族,上下文感知)
   → 回执落位 → render → ImeView

第二路 MagicFlow(magic.rs)
   同步数据流:# 前缀触发(Idle # 进 Snippet;X'#cmd 链式)——查询/选中/强触发
   异步数据流:后台动作完成后 IoThread 注入 Async(MagicTick) 驱动 Magic 状态机转移
   不依赖 ControlPane —— 只吃 SessionState;家族级资源(voice/req/snippets/expander)
   由引擎持有的 MagicFamily 经 StepEnv 注入(成员实例每会话一份,在 MagicSession)
```

## 二、落地内容

- **状态拆分**(state.rs):`StateMachine` 拆为
  - `SessionState`(纯数据:state/comp/panel/magic/context/ctx/
    candidate_meta)—— 双路键处理的唯一交互面,**不依赖 ControlPane**;
  - `ControlPane`(系统控制:session + flags 镜像)—— 三大事件分发、
    action 归一化、`resolve` 编排移入 pre.rs(stage1 自由函数)。
- **双路归位**:family.rs = 第一路 FamilyPrediction(头文档 + 路由矩阵);
  magic.rs = 第二路 MagicFlow(同步/异步双数据流头文档);impl 全部挂在
  `SessionState` 上。
- **后处理四步显式化**(post.rs postprocess 文档):①家族间综合评分
  ②评分微调 ③造词单字区 ④输入统计与上下文构建(位置在提交路径与
  collect 注入点,注明)。
- **ctx 归位**:ctx 是会话数据(每会话一个挂号),从 ControlPane 移入
  `SessionState`;引擎 `with_ctx` 写 `pane.session.ctx`。
- engine `Session` 字段 `sm` → `pane`;只读转发走 `session()(&self)`,
  配置写走 `session_mut()`。

## 三、验证

- 全测试绿(149+21+7+11+14+2);clippy 双 crate 0 警告;
- 评测持平:tc_dict_sample 98.0 / tc_en_sample 99.4。
