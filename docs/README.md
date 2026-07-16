# audio-aura 文档索引

> 文档分组 + 状态标签。**先读 `architecture.md`**（现状权威）。北极星：系统级 AI 秘书（geek-familiar 悬浮窗 + 语音(audio-aura) + 视觉/操作(visual-rover) agent team）。

## 📍 现状（必读）
- **[architecture.md](architecture.md)** — as-built：6-crate 脊柱拓扑 + 三阶段抽象（Stage1Executor/Stage2Calibrator/Stage3Agent）+ 数据流 + 运行方式。`🟢 当前`
- **[roadmap.md](roadmap.md)** — 未完成事宜，按近/中/长期排列 + 工程债 + 决策待定。`🟢 当前`

## 🧭 决策（why this, not that）
- **[runtime-selection.md](runtime-selection.md)** — 双运行时（ONNX 侧 sherpa-onnx / HF 侧 mistral.rs），各管各的、只通过文本交互。`🟢 当前`

## 🎯 优化与前瞻
- **[stage2-optimization.md](stage2-optimization.md)** — Stage2 校准优化手段全集 + 优先级/落地状态（1.1–1.6 全 ✅，ASR 层热词同音词 ✅）。`🟢 当前`
- **[adaptive-learning.md](adaptive-learning.md)** — 自适应学习三阶：热词积累 → 检索增强(RAG) → LoRA 微调。把误读（Bevy）真正绑死的长线路线。`🟢 前瞻`

## 📚 参考
- **[livekit-port-notes.md](livekit-port-notes.md)** — LiveKit agents 管线研究（VAD/turn-detection/流式接力/barge-in），可移植设计提炼。`🟢 参考`
- **[ldconfig.md](ldconfig.md)** — 动态链接/rpath 第一性原理（链接期 vs 运行期、`$ORIGIN`、传递依赖）。本项目 sherpa `.so` 经 `lib/` + RUNPATH 自定位的依据。`🟢 参考`

## 🏛 历史（已被实现取代，留作存档）
- **[stage1-2-problems.md](stage1-2-problems.md)** — 2026-07-12 的 4 个实时录音问题（丢句/缓冲/流式纠偏/上下文）。`⚪ 已全部解决`，见 architecture.md。
- **[index.md](index.md)** — 早期 LiveKit "级联语音 agent" brainstorm。`⚪ 已被 architecture.md / runtime-selection.md 取代`。
- **[chat.txt](chat.txt)** — 最初的设计蓝图口述。`⚪ 历史存档`。

---

## 一句话定位

audio-aura = **语音助手前端 + 中间守护进程**：下接 omni-scout 录音，上接 geek-familiar（socket）。用 AI-agent 手段把 ASR 准确率榨到极致（三阶段提交 + 可选的带工具元 agent），是系统级 AI 秘书的"耳朵 + 整流 + 意图"层。
