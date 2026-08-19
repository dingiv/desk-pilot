# aura 文档索引

> **先读 `architecture.md`**（现状权威）。北极星：系统级 AI 秘书（desk-pilot）。

## 📍 现状（必读）

- **[architecture.md](architecture.md)** — as-built：crate 拓扑 + 三阶段 + dp-models 通用模型提供库 + 模型选型（本地/远程）+ 双运行时决策 + 配置。
- **[stages.md](stages.md)** — Stage1（边界范式 VadSegment/VadWindow）与 Stage2（窗口联合整流）的能力、流程、事件契约、设计沿革（D1-D4）。
- **[roadmap.md](roadmap.md)** — 已完成 / 近期 / 中期 / 长期。
- **[client-state-sync.md](client-state-sync.md)** — 客户端状态同步：控制面 / 数据面 / 按需，5 事件协议。

## 🧭 设计参考

- **[real-world-speech-design.md](real-world-speech-design.md)** — 真实场景 8 类语音问题 + 处理层 + 状态。
- **[adaptive-learning.md](adaptive-learning.md)** — 自适应学习闭环设计（存疑→纠错→热词/微调，护城河）。
- **[voice-models-research.md](voice-models-research.md)** — 实时语音（S2S/TTS）模型调研 + Moshi spike 记录（已搁置）。
- **[livekit-port-notes.md](livekit-port-notes.md)** — LiveKit 移植笔记（历史，已大部分被边界范式取代）。

## 一句话定位

aura = **语音助手前端 + 中间守护进程**：下接 omni-scout 录音，上接 geek-familiar 秘书。
用三阶段提交（ASR → 纠偏 → 工具）把语音识别准确率榨到极致，是系统级 AI 秘书的"耳朵 + 整流"层。
