# Agent 模块

visual rover 自带的 agent 循环，用于提供标准的 gui agent 的实现展示。


## AgentTeam
外界管理的 agent 模块的第一句柄，把这个 AgentTeam 实例当作一个大型状态机


## LeaderAgent
当一个 AgentTeam 仅有一个 Agent 的时候，该 Agent 作为 LeaderAgent，直接与用户对话；

## WorkerAgent

## Agent
Agent 统一接口

先创建一个 Agent 实例，此时并没有任何任务会开始执行，保持无副作用产生。


其中一个非常重要的属性 Platform —— 由外界传递。


多 Agent，多后端大模型

Agent 之间添加协作机制；
+ Agent 有自我介绍，向其他 Agent 展示自己的强项和能力，担任的事务；



## Task
代表用户的一个请求和任务

在任意时刻，一个 Agent/SubAgent 只能运行一个 Task,

但是，SubAgent 和 Task 并没有一对一的绑定关系，一个 Task 可以由一个 Agent 做，也可以由多个 Agent 传递着做，还可以两个 Agent 一起做；但是多数情况下是一个 Agent 先做一个 Task, 然后再做另一个 Task


## Model
一个大模型，包含 api-key 等等，内部自行实现连接

一个 Agent 包含一个或者多个 Model

## ContextManager
被 Task 关联，一个 Task 对应一个 ContextManager


## PromptBuilder
负责从多个数据源中构造出一个完整的提示词


## MemoryManager


## MemoryStore
负责持久化记忆和全局文件管理，单例对象，一个 Agent 关联一个 MemoryStore