# aura 文档索引

> **先读 `architecture.md`**（现状权威）。北极星：系统级 AI 秘书（desk-pilot）。

## 📍 现状（必读）

- **[architecture.md](architecture.md)** — as-built：crate 拓扑 + 三阶段 + dp-models 通用模型提供库 + 模型选型（本地/远程）+ 双运行时决策 + 配置。
- **[stages.md](stages.md)** — Stage1（边界范式 VadSegment/VadWindow）与 Stage2（窗口联合整流）的能力、流程、事件契约、设计沿革（D1-D4）。
- **[roadmap.md](roadmap.md)** — 已完成 / 近期 / 中期 / 长期。
- **[client-state-sync.md](client-state-sync.md)** — 客户端状态同步：控制面 / 数据面 / 按需，5 事件协议。

## 🧭 设计参考

- **[pipeline-optimization.md](pipeline-optimization.md)** — 整条语音识别管线优化设计(设计,未实现):瓶颈地图 + 分层方案 P0–P4(P0=batch 异步化;P1=Stage2 整流去冗余/修 O(n²) 与定稿积压延迟;P2=短段免重跑;P3=首字延迟;P4=鲁棒性),全部收敛在 aura-core、前端零改动。
- **[async-batch-design.md](async-batch-design.md)** — 把 batch 识别移出消费线程(已实现,pipeline-optimization.md 的 P0):根除"间隔 1–3.5s 首句被吞"的过早切段 bug;单 batch worker + readiness 定稿 + Stage2 去状态化,wire 协议零改动;线程创建全部收归 pipeline.rs。
- **[real-world-speech-design.md](real-world-speech-design.md)** — 真实场景 8 类语音问题 + 处理层 + 状态。
- **[adaptive-learning.md](adaptive-learning.md)** — 自适应学习闭环设计（存疑→纠错→热词/微调，护城河）。
- **[voice-models-research.md](voice-models-research.md)** — 实时语音（S2S/TTS）模型调研 + Moshi spike 记录（已搁置）。
- **[livekit-port-notes.md](livekit-port-notes.md)** — LiveKit 移植笔记（历史，已大部分被边界范式取代）。

## 一句话定位

aura = **语音助手前端 + 中间守护进程**：下接 omni-scout 录音，上接 geek-familiar 秘书。
用三阶段提交（ASR → 纠偏 → 工具）把语音识别准确率榨到极致，是系统级 AI 秘书的"耳朵 + 整流"层。
