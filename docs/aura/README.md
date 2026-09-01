# aura 文档索引

> **先读 `architecture.md`**(现状权威)。北极星:系统级 AI 秘书(desk-pilot)。

## 📍 现状(必读)

- **[architecture.md](architecture.md)** — as-built:crate 拓扑(pipeline/ 文件夹化)+ 执行模型 + dp-models 通用模型提供库 + 模型选型(本地/远程)+ 双运行时决策 + 配置。
- **[pipeline.md](pipeline.md)** — 双阶段流水线架构全文:两 Stage/两间隔、时间轴工作原理、事件协议与四不变式、执行模型与通道清单、降级链、前端级联折叠。
- **[stages.md](stages.md)** — Stage1(边界范式 VadSentence/VadParagraph)与 Stage2(联合整流)的能力、流程、wire 契约、设计沿革(D1-D4)。
- **[client-state-sync.md](client-state-sync.md)** — 客户端状态同步:控制面/数据面/按需,5 事件协议。
- **[new-pipeline.md](new-pipeline.md)** — 施工中:Stage0/Stage1 分界重构(Stage0VAD trait、VAD 下沉拉流线程、FrontEvent 队列、大脑事件驱动化,R1-R4 分轮)。
- **[roadmap.md](roadmap.md)** — 已完成/近期/中期/长期。

## 🧭 设计参考

- **[debugging.md](debugging.md)** — 排障手册:留痕对表 + 症状→根因速查 + round 简表(合并自原 issues/async-rewrite/optimization/async-batch 四文,2026-08-31)。
- **[real-world-speech-design.md](real-world-speech-design.md)** — 真实场景 8 类语音问题 + 处理层 + 状态。
- **[adaptive-learning.md](adaptive-learning.md)** — 自适应学习闭环设计(存疑→纠错→热词/微调,护城河)。
- **[voice-models-research.md](voice-models-research.md)** — 实时语音(S2S/TTS)模型调研 + Moshi spike 记录(已搁置)。
- **[livekit-port-notes.md](livekit-port-notes.md)** — LiveKit 移植笔记(历史,已大部分被边界范式取代)。

## 一句话定位

aura = **语音助手前端 + 中间守护进程**:下接 omni-scout 录音,上接 geek-familiar 秘书。
用两阶段识别(流式粗稿 → batch 精稿 → LLM 双通道纠偏)把语音识别准确率榨到极致,
是系统级 AI 秘书的"耳朵 + 整流"层。
