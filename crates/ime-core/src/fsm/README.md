# fsm — round13 架构(实现映射)

```
ControlPane(src/fsm/control.rs,原 StateMachine)
   -> 分发三大事件类型(handle_event):
      ① 普通键        → 第一路 FamilyPrediction
      ② `#` 触发键     → 第二路 MagicFlow 同步数据流
         异步信号(IoThread 注入 Async(MagicTick)) → 第二路 MagicFlow 异步数据流
      ③ 系统控制事件(Control)→ 提前拦截并返回(提交/选词/翻页/复位),不进双路
   -> 处理完成统一封装 ImeView 返回(action 归一化 + flags 镜像)

   -> 双路键处理(都只吃 SessionState 会话纯数据,不依赖 ControlPane)
        -> 第一路 FamilyPrediction(src/fsm/family_prediction.rs,原 family.rs)
           三个家族对象(pinyin/english/emoji Arc)自己有状态,跨会话共享,
           由引擎持有,经 StepEnv 注入
            -> 后处理(src/fsm/post.rs 纯函数):
               家族间综合评分(merge) / 评分微调(置顶+过滤链) / 造词单字区 /
               输入统计(提交学习 record_pick/L0) /
               上下文构建(InputContext 经 scorer.collect 在预测前注入,
               各家族上下文感知)
        -> 第二路 MagicFlow(src/fsm/magic_flow.rs,原 magic.rs)
           同步数据流:# 前缀触发(Idle # 进 Snippet;X'#cmd 链式)
           异步数据流:后台动作完成后 IoThread 注入异步事件,驱动 Magic 状态机转移
   -> 路由层编排(src/fsm/pre.rs ControlStage + resolve):交付件 → stage3 → 落位
```

## 文件映射

| 文件 | 内容 |
|---|---|
| `control.rs`(原 state.rs) | `ControlPane`(系统控制分发)+ `SessionState`(会话纯数据) |
| `family_prediction.rs`(原 family.rs) | 第一路:打分家族预测 + 交付件产出/落位 + Composition/CandidatePanel/StepEnv |
| `magic_flow.rs`(原 magic.rs) | 第二路:Magic 命令同步/异步双数据流 + MagicSession |
| `post.rs` | stage3 纯函数管道:postprocess/render/pending_commit_text + 交付件契约 |
| `pre.rs` | ControlStage 键路由矩阵 + resolve 编排 |
| `event.rs` | ImeEvent 三分类(Key/Control/Async) |
| `chain.rs` / `key.rs` | 链式段解析 / 键枚举与标志位(不变) |
