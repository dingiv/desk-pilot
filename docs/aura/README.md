# audio-aura 文档索引

> **先读 `architecture.md`**（现状权威，2026-07-17）。北极星：系统级 AI 秘书（desk-pilot = geek-familiar 精灵 + audio-aura 语音 + visual-rover 视觉/操作 agent team）。

## 📍 现状（必读）
- **[architecture.md](architecture.md)** — as-built：crate 拓扑（aura-asr/store/dcl/core/agent/daemon）+ 三阶段提交 + 线程模型 + 配置/日志/存储 + 运行方式。`🟢 当前 2026-07-17`
- **[roadmap.md](roadmap.md)** — 未完成事宜（近 R2/R4/R5/R6 / 中 M1–M6 / 长 L1–L4）+ 工程债 + open 决策。`🟢 当前`

## 🧭 决策（why this, not that）
- **[runtime-selection.md](runtime-selection.md)** — 双运行时（ONNX sherpa-onnx / HF mistral.rs），各管各、仅文本交互。`🟢 当前`

## 🎯 优化与前瞻
- **[stage2-optimization.md](stage2-optimization.md)** — Stage2 校准优化全集（1.1–1.6 全 ✅）+ ASR 层热词同音词。`🟢 当前`
- **[adaptive-learning.md](adaptive-learning.md)** — 自适应学习三阶：热词 → RAG → LoRA。`🟢 前瞻`

## 📚 参考
- **[livekit-port-notes.md](livekit-port-notes.md)** — LiveKit agents 管线研究（VAD/流式接力/barge-in）。`🟢 参考`
- **[ldconfig.md](ldconfig.md)** — 动态链接/rpath 原理；sherpa `.so` RUNPATH 自定位依据。`🟢 参考`

## 🏛 历史（已被实现取代，存档）
- **[stage1-2-problems.md](stage1-2-problems.md)** — 2026-07-12 实时录音四大问题 + 2026-07-17 补章（隐蔽丢音四因复盘 + 真麦复测后 P1–P4）。`⚪ 全部已解决`
- **[index.md](index.md)** — 早期 LiveKit "级联语音 agent" brainstorm。`⚪ 被 architecture.md / runtime-selection.md 取代`
- **[chat.txt](chat.txt)** — 最初的设计蓝图口述。`⚪ 历史存档`

---

## 相关子系统文档

- 视觉/操作（visual-rover + omni-scout）：`../scout-server.md`、`../som.md`、`../design.md`、`../decisions.md`
- 桌面精灵（geek-familiar）：`../familiar/index.md`
- 根索引：`../README.md`

## 一句话定位

audio-aura = **语音助手前端 + 中间守护进程**：下接 omni-scout 录音，上接 geek-familiar（socket）。用 AI-agent 手段把 ASR 准确率榨到极致（三阶段提交 + 可选的带工具元 agent），是系统级 AI 秘书的"耳朵 + 整流 + 意图"层。
