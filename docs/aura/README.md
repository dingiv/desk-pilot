# aura 文档索引

> **先读 `architecture.md`**（现状权威，2026-08）。北极星：系统级 AI 秘书（desk-pilot）。

## 📍 现状（必读）

- **[architecture.md](architecture.md)** — as-built：crate 拓扑（合并后）+ 三阶段 + dp-models + 双运行时。`🟢 当前`
- **[roadmap.md](roadmap.md)** — 已完成/近期/中期/长期。`🟢 2026-08 更新`

## 🧭 设计参考

- **[runtime-selection.md](runtime-selection.md)** — 双运行时（ONNX sherpa / HF mistral.rs）选型依据。
- **[adaptive-learning.md](adaptive-learning.md)** — 自适应学习闭环设计（存疑→纠错→热词/微调）。
- **[real-world-speech-design.md](real-world-speech-design.md)** — 真实场景 8 类语音问题 + 处理方案。`🟢 2026-08 新`
- **[realtime-voice-models.md](realtime-voice-models.md)** — 实时语音模型调研（Moshi/GLM-4-Voice/Mini-Omni2）。`🟢 2026-08 新`
- **[moshi-spike.md](moshi-spike.md)** — Moshi spike 记录（16GB VRAM 限制搁置）。

## 📚 技术参考

- **[livekit-port-notes.md](livekit-port-notes.md)** — LiveKit 全双工/流式接力/barge-in 研究。

## 一句话定位

aura = **语音助手前端 + 中间守护进程**：下接 omni-scout 录音，上接 geek-familiar 秘书。
用三阶段提交（ASR → 纠偏 → 工具）把语音识别准确率榨到极致，是系统级 AI 秘书的"耳朵 + 整流"层。
