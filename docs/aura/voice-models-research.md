# 实时语音模型调研 + Moshi spike（2026-07，历史）

> 状态：📋 **调研 + spike 记录（已搁置）**。Moshi 全双工 7B 在 16GB VRAM 跑不了；
> 当前走级联管道优化（路线 B）。本文是 S2S 模型全景 + spike 失败记录，未来硬件升级
> （24GB+）时参考。

## 背景：延迟瓶颈

调研时管道 ~2.2s（VAD 等 1s 静音 → batch ASR 862ms → Stage2 300ms → TTS 占位）。
2026-08 边界范式后感知延迟大幅改善（流式 partial ~0.5s 出字）；剩余瓶颈在 TTS（占位）。
两条路线：
- **A：端到端 S2S**（替换整个管道）→ ~200-500ms
- **B：流式管道优化**（保留级联 + 补 TTS）→ ~400ms —— 当前走这条

## S2S 模型全景

| 模型 | 延迟 | 中文 | 显存 | Rust | 全双工 | 特点 |
|---|---|---|---|---|---|---|
| **Moshi** (Kyutai) | 200ms | ⚠️ 英文 | 16GB | ✅ | ✅ | 最低延迟，Rust 驱动 |
| **GLM-4-Voice** (智谱) | ~500ms | ✅ 原生 | 24GB | ❌ | ❌ | 中文最好，情绪/方言控制 |
| **Mini-Omni2** | ~300ms | ✅ | 消费级 | ❌ | ❌ | 语音+视觉双模态 |
| **Step-Audio-AQAA** | ~400ms | ✅ 中英日 | ~16GB? | ❌ | ❌ | 角色扮演+推理 |

## TTS 模型（级联管道补齐）

| 模型 | 延迟 | 中文 | 显存 | 特点 |
|---|---|---|---|---|
| **Kokoro** | ~100ms | ✅ | 极低 | 2026 公认最佳开源 TTS（M2 首选） |
| Qwen3-TTS | 流式 | ✅ | ~8GB | 端到端因果设计 |
| Chatterbox | ~150ms | ✅ | 低 | 生产级 |

## Moshi spike 记录（搁置）

**目标**：验证 Moshi 200ms 全双工在 desk-pilot 的可行性（RTX 5070 Ti 16GB / CUDA 13.2 / sm_120）。

**结果：全部失败**：
| 路径 | 失败 |
|---|---|
| Python BF16 GPU | OOM（7B × BF16 = 14GB 权重，16GB 不够 warmup） |
| Python Q8 | bitsandbytes dtype bug（weight_scb bfloat16 预期 float32） |
| Rust candle GPU | cudarc 0.16.2 不认 CUDA13.2（可 patch）+ candle 0.9.1 无 sm_120 kernel（不可 patch）+ sentencepiece-sys GCC15 编译失败 |

**解锁条件**：24GB+ 显存 / bitsandbytes Q8 修复 / candle 0.9.1→0.10+（Moshi 锁版本）/ GCC15 patch。

**建议**：搁置 Moshi；专注级联管道优化（补 Kokoro TTS ~100ms 后总延迟 ~400ms）；关注 Hibiki（更轻）。

## 参考资料

- [Moshi GitHub](https://github.com/kyutai-labs/moshi) · [Moshi 论文](https://arxiv.org/abs/2410.00037)
- [GLM-4-Voice](https://github.com/zai-org/GLM-4-Voice) · [Artificial Analysis S2S](https://artificialanalysis.ai/speech-to-speech)
- Moshi 仓库：`/workspaces/gui_agent/moshi/`（cudarc patch 在 cargo registry）
