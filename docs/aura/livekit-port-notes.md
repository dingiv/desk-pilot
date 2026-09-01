# LiveKit Agents → Rust 移植笔记（历史）

> **状态**：📋 施工蓝本，**大部分已被边界范式取代**（2026-08-17）。仅保留"采纳 vs 跳过"
> 结论与参考来源；详细设计见 `pipeline.md`。B 部分（流式接力/barge-in）等 TTS 真后端
> （M2 Kokoro）再动工。

来源：精读 `/workspaces/gui_agent/livekit-agents`（Python）。LiveKit ≈ 我们的 Stage1+Stage2
的成熟实现，**没有 Stage3（topic 长期记忆）**——那是我们的差异化。本地单用户改写：去掉
WebRTC/房间/多参与者，音频源为 omni-scout HTTP。

## 采纳 vs 跳过

**采纳（已落地或受影响）**：
- VAD 切段 + 双遍识别（流式 partial + VAD 门控批式 final）——落成边界范式的
  `VadSegment`/`VadWindow` + 段级/窗口级 batch。
- VAD 滞回参数（min_speech / min_silence）——在 `vad.min_silence` 等配置。
- 断句状态机（min/max delay + 语义 turn-detector）——**未照搬**：用 `min_silence`（切段）
  与 `merge_gap`（合并窗口）解耦替代（见 `pipeline.md`）。
- 并发原语映射（Future→oneshot / Event→Notify / Task→JoinHandle）——B 部分用。

**跳过**：WebRTC/SFU/房间/多参与者/AEC（本地单用户）；语义 turn-detector（中文 EOU
阈值那套，未采纳）；动态 endpointing。

**我们独有**：Stage3 topic 记忆——Stage2 整流后的节点喂话题切分 + 摘要，作 agent team
长期上下文（"秘书 vs 金鱼"分水岭）。

## B 部分（未动工）：流式接力 + barge-in

秘书语音回应 + TTS 环（等 M3 本地 TTS）：
- 流式接力：LLM 块 → 文本 channel → 按句边界切段 → 每段独立起 TTS，首块即播。
- barge-in：`SpeechHandle{interrupt, done, allow_interruptions}`；用户出声中断 + 假打断
  （pause + 2s 计时，没续说就 resume）。
- 调度器：`BinaryHeap<SpeechHandle>` + `Notify` + `AgentState` 状态机。

## 参考

关键源文件：`livekit/agents/vad.py`、`voice/audio_recognition.py`、`voice/endpointing.py`、
`agents/inference/eot/`（turn-detector）、`stt/stream_adapter.py`、`voice/speech_handle.py`。
