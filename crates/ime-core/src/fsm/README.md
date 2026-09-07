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


## 新增 OverlayDict

磁盘上

SeedDict
OverlayDict
MergedDict
Wordbook: 拼音 和 英文
OverlayData


内存中

OverlayData

Wordbook 需要新增词频统计, 

MergedDict

拼音英文在预测的时候，使用的是merged dict, 同时对 OverlayData 做一次相同的动作; 如果通过 OverlayData 中存在相同的预测项目, 则最终该项目的预测权重与

重新规划单词本的格式, 改成纯文本;


每一个单词项需要记录初始权重值;
 

好，我们继续阐述需求啊，先设计; 

就是说啊，现在。我们的端到端需求是说，当我们现在。哎，重复的输入了某些单词之后，那么在之后的预测中啊，这些单词的权重将会得到增强。那么，同时呢，为了支持用户自生词，那么我们还需要新增自生词模块，将那些没有在系统词典中预设的这些词汇也加入到。使用统计当中。

在我们的设计当中，系统预设的seed词典也好，还是用户后来添加的词典也好，那么它是一个死的词，它并不是为某一个用户专门使用所优化的词。我们的overlay设计就是为了实现这个单用户专一优化的能力;

为此呢，我们需要引入这样一些模块：一个呢就是系统seed词典，一个呢就是我们的overlay词典。系统词典是不变的，而OverlayData它是一直在变的。但是呢，我们的使用过程中会发现一个问题。一直在变，可能会带来性能问题，因此呢，我们需要引入了三级架构。

一层架构呢，是在内存中啊实时进行计算的。那么这些词汇的数量呢比较少，但是呢，它走全套的重建流程，没有一个新的词语进来之后呢，它就会重建一次，那么从而来调整它的频率。

另一层架构呢，是持久化了的overlay数据。那么，它的数据量是比较大的。每一次启动冷加载, 现在呢，我们新的设计之后呢，就它就不需要再和seed dict进行合并了。它单独作为一层。它单独作为一层。然后呢？每次当……Overlay data它的数量达到某一个级别的时候呢？它就从 overlay data 里面持久化到 overlay dict 里面。一旦被持久化到 overlay dict 里面，我们就需要将当前 overlay data 里面的数据进行清空，因为这里面的数据已经被移动到 overlay dict 里面去了。

 那最后一层呢，就是我们的系统seed词典。这一层是不可变的。