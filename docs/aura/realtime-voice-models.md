# 实时语音模型调研（2026-07）

> 状态：📋 调研 + 选型。目标：降低 aura 语音管线的端到端延迟（当前 ~2.2s），探索全双工
> 打断能力。北极星：[[ai-secretary-north-star]]。

## 背景：当前管道的延迟瓶颈

```
（2026-07 调研时）用户说话 → VAD 等 1s 静音 → batch ASR 862ms → Stage2 300ms → TTS(占位)
         共 ~2.2s 才出回应
```

**2026-08-17 现状**：碎片合并 + 勤快 Stage2 落地后，感知延迟已大幅改善——
流式 partial ~0.5s 出字（说话中持续）、Stage2 每 1s 校准一次 partial、每碎片
（≥1s 停顿）出临时纠偏句；只有**权威定稿**仍需等 merge_gap（2.5s）静默。
纯响应延迟的剩余瓶颈在 TTS（仍占位 NoopTts）。

两条优化路线：
- **路线 A：端到端 S2S**（替换整个管道）→ ~200-500ms
- **路线 B：流式管道优化**（保留级联架构 + 补 TTS）→ ~400ms

## 端到端 Speech-to-Speech 模型

| 模型 | 延迟 | 中文 | 显存 | Rust | 全双工 | 特点 |
|---|---|---|---|---|---|---|
| **Moshi** (Kyutai) | 200ms | ⚠️ 英文 | 16GB ✅ | ✅ Rust/C++ | ✅ | 最低延迟，有 Rust 驱动，可在 iPhone 15 Pro 跑 |
| **GLM-4-Voice** (智谱) | ~500ms | ✅ 原生 | 24GB ❌ | ❌ Python | ❌ 半双工 | 中文最好，情绪/方言/语速控制 |
| **Step-Audio-AQAA** (阶跃) | ~400ms | ✅ 中英日 | ~16GB? | ❌ Python | ❌ | 2025.6 新出，角色扮演+推理 |
| **Mini-Omni2** | ~300ms | ✅ 中英 | 消费级 ✅ | ❌ Python | ❌ | 语音+视觉双模态，轻量 |
| **PersonaPlex** (NVIDIA) | 205ms | ❌ 英文 | NVIDIA 专用 | ❌ | ✅ | 音色克隆，硬件绑死 |
| **MOSS-TTS-Realtime** | ~300ms | ✅ | ? | ❌ Python | ❌ | 多轮上下文感知 |

## 纯 TTS 模型（补齐级联管道最后一块）

| 模型 | 延迟 | 中文 | 显存 | 特点 |
|---|---|---|---|---|
| **Kokoro** | ~100ms | ✅ | 极低 | 2026 公认最佳开源 TTS，轻量 |
| **Qwen3-TTS** | 流式 | ✅ | ~8GB | Qwen3 家族，端到端因果设计，流式低延迟 |
| **Chatterbox** (Resemble AI) | ~150ms | ✅ | 低 | 生产级，高性能 |

## 选型决策

### 当前 spike：Moshi（路线 A 验证）

**选 Moshi 的理由**：
1. **Rust 原生驱动**——直接进 `crates/aura-asr/`，不需 Python 进程
2. **16GB 刚好**——RTX 5070 Ti 够用
3. **200ms 全双工**——碾压当前 2.2s 管道 + barge-in 打断（AI 秘书圣杯）
4. **英文先 spike**——验证 Rust 集成 + 全双工架构，中文后续可换 GLM-4-Voice

**Moshi 的风险**：
- 英文为主，中文弱
- 端到端模型，替换 Stage1+Stage2+Stage3（丢失纠偏/热词/用户纠正等精确控制）
- 和现有管道并列（"实时模式" vs "精准模式"），不冲突

### 后续计划

| 优先级 | 做什么 | 依赖 |
|---|---|---|
| ✅ 当前 | Moshi spike（Rust 集成 + 延迟验证） | 16GB GPU ✅ |
| 近期 | Kokoro TTS（补齐级联管道） | 轻量 |
| 中期 | GLM-4-Voice（换大卡后） | 24GB+ 显存 |
| 远期 | Mini-Omni2 / Step-Audio（中文 S2S） | 多模态需求 |

## Moshi 技术概况

- **架构**：深度严格遵守 Transformer（Mimi 音频编码器 + 7B 文本推理 LLM + Helium TTS 解码器）
- **全双工**：同时编码输入音频流 + 生成输出音频流（帧级交错），支持打断
- **延迟**：200ms（流式，首字节 ~160ms）
- **Rust 驱动**：`moshi-rs` crate（kyutai-labs 官方维护）
- **模型文件**：HuggingFace `kyutai/moshiko` / `kyutai/moshillama`

## 参考资料

- [Moshi GitHub](https://github.com/kyutai-labs/moshi)
- [Moshi 论文](https://arxiv.org/abs/2410.00037)
- [Best Speech-to-Speech Model 2026](https://inworld.ai/resources/best-speech-to-speech-model)
- [GLM-4-Voice GitHub](https://github.com/zai-org/GLM-4-Voice)
- [Artificial Analysis S2S Benchmark](https://artificialanalysis.ai/speech-to-speech)
