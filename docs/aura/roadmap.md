# 路线图（2026-08 更新）

> 对照 `architecture.md` 现状。北极星：[[ai-secretary-north-star]]。

## ✅ 已完成

- [x] crate 合并（aura-core + aura-dcl + aura-store → 一个 aura-core）
- [x] dp-models Provider 抽象（AsrProvider/LlmProvider/VlmProvider + Http* remote）
- [x] daemon asr_kind/llm_kind local/remote 配置切换（aura.yaml）
- [x] Stage2 简化：纯文本输出（无 JSON）、PromptBuilder 精简、模型换 Qwen2.5-3B-Instruct
- [x] ContextWindow 禁用（3B 复读问题）
- [x] 用户纠偏：POST /api/correct → CorrectionStore → Stage2 corrections 段注入
- [x] Web UI 编辑纠正（UtteranceList inline edit + SSE correction event）
- [x] Qwen3-ASR strip_qwen3_markers（修标记泄漏）
- [x] Ring 空超时 → 喂静音帧触发 VAD EOS（修 batch 不触发 bug）
- [x] ASR 后端：SenseVoice/Whisper/Qwen3-ASR（onnx）+ remote HttpAsr
- [x] aura.json → aura.yaml（YAML 支持注释）
- [x] assets/ 统一（models + sherpa + cudnn + lib）

## 🔴 近期

| # | 事项 | 价值 | 说明 |
|---|---|---|---|
| R1 | **自适应 VAD**（min_silence 动态化） | 极高 | 短句 1.0s / 中长句 1.8s — 减少"停顿碎片化" |
| R2 | **段合并**（EOS 后 pending 0.8s） | 极高 | 连续语音拼接为整句，消除碎片 |
| R3 | **延迟显示**（合并+纠偏后一次性 final） | 高 | 消除"焦虑感"——流式 partial 正常，final 延迟到合并后 |
| R4 | **口误自纠检测**（正则 "X 不对 Y" → Y） | 中 | 零 LLM 开销的确定性预处理 |

## 🟡 中期

| # | 事项 | 价值 | 说明 |
|---|---|---|---|
| M1 | **geek-familiar 接入** | 极高 | Stage3 调度替换规则触发器，闭合北极星 |
| M2 | **TTS 真模型**（Kokoro） | 高 | NoopTts → 真后端，语音回读（~100ms） |
| M3 | **Stage2 上 vLLM/sglang** | 中 | llm_kind=remote + spec decoding（7B+ 模型，重开 ContextWindow） |
| M4 | **Stage3 能力**（FineTuner/Summarizer/MemoryStore） | 中 | 当前只有 HotwordManager + CorrectionStore |
| M5 | **Stage3→Stage1 ASR 热词反馈** | 中 | 重建 recognizer 动态烘焙热词 |
| M6 | **Moshi 全双工 spike**（Rust 集成） | 中 | 待 candle 升级支持 sm_120 + sentencepiece 编译修复 |

## 🟢 长期

| # | 事项 | 价值 |
|---|---|---|
| L1 | visual-rover agent team 协同（语音 intent→视觉/操作任务） | 北极星 |
| L2 | 桌面宠物常驻秘书闭环（听→整流→意图→派活→汇报→TTS） | 北极星 |
| L3 | 自适应学习闭环（标注→热词→RAG→LoRA） | 护城河 |
| L4 | 全双工可打断（Moshi / GLM-4-Voice） | 体验飞跃 |
