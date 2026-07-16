# LiveKit Agents → Rust 移植笔记（Stage1/2 施工蓝本）

来源：精读 `/workspaces/gui_agent/livekit-agents`（Python）。LiveKit ≈ 我们的 Stage1+Stage2（实时听+答）的成熟实现，**没有 Stage3（topic 长期记忆）**——那是我们的差异化。这里提炼其可移植设计。全部为**本地单用户**改写：去掉 WebRTC/房间/多参与者，音频源改为 omni-scout HTTP 或浏览器 PCM。

关键源文件：`livekit/agents/vad.py`、`voice/audio_recognition.py`、`voice/endpointing.py`、`agents/inference/eot/`（turn-detector）、`stt/stt.py`、`stt/stream_adapter.py`、`voice/agent_activity.py`、`voice/generation.py`、`voice/speech_handle.py`。

---

## A. Stage1 = VAD + 断句 + 流式 ASR（M2 直接实现）

### 1) 音频帧 / VadStream / StreamAdapter（核心抽象）
- **AudioChunk**：`{ sample_rate:u32, channels:u16, samples:u32, pcm:Vec<i16> }`（s16le mono；duration = samples/sr）。帧节奏 **20ms**（VAD 窗 512 样本=32ms@16k）。
- **VadStream trait**：`push(chunk)` / `poll_event -> VadEvent{kind:StartOfSpeech|EndOfSpeech, audio:Vec<AudioChunk>}`。EndOfSpeech 事件**携带累积的整段音频帧**（关键：供批式 ASR 用）。
- **StreamAdapter（批→流）**：把批式识别器（SenseVoice/Whisper）用 VAD 包成流式——SOS→发 StartOfSpeech；累积帧；EOS→`merge_frames` 后 `batch.recognize()` → 发 FinalTranscript。**这正是 SenseVoice + earshot VAD 的用法**。流式识别器（sherpa Zipformer）则同时发 Interim（每步）+ Final（端点）。
- **SpeechEvent**：`kind: Interim|Final|StartOfSpeech|EndOfSpeech`，`utterance:{text,confidence,start_ms,end_ms,words?}`。
- Rust：`audio-aura-asr` crate。`trait BatchAsr`/`trait StreamingAsr`（sherpa-onnx 离线/在线）、`trait Vad`（earshot 或 silero via `ort`）、`VadSegmenter`（StreamAdapter 等价）。channel 传所有权（`tokio::mpsc`），别借用。

### 2) VAD 参数（Silero，直接照抄）
`activation=0.5, deactivation=max(act-0.15,0.01)=0.35`（滞回）；`min_speech=50ms, min_silence=550ms`；窗 32ms；概率 `ExpFilter(alpha=0.35)` 平滑；两个计数器（speech/silence 时长，切换即清零）跨阈值才发事件。**earshot 纯 Rust 更省事**（0 依赖），Silero-onnx 精度略高、可 `ort` 跑。

### 3) 断句状态机（比固定超时强的关键）
`voice/endpointing.py`：
- VAD 发 EndOfSpeech → 起 EOT 计时，**先按 `min_delay=0.5s`**（流式默认 0.3s）。
- 期间并行跑**语义 turn-detector**：EOU 概率 `< 阈值`（说明"没说完"）→ **延到 `max_delay=3.0s`**（流式 2.5s）；否则 0.5s 提交。
- 计时 = `delay - 已过静音`，睡够即 `commit_turn()`。
- turn-detector 触发点：VAD `INFERENCE_DONE` 且累积静音 ≥200ms 就开跑（抢跑）。
- **DynamicEndpointing**（可选）：两个 `ExpFilter` 学习"句间/轮间停顿"自适应 min/max。
- **turn-detector 模型**：`livekit/turn-detector` 的 `model_q8.onnx`（Qwen2.5-0.5B 蒸馏，取**最近 6 轮 / 128 token 左截断**，输出 EOU 概率）；**中文阈值 0.3550**（现成，见 `inference/eot/languages.py`）；Rust 用 `ort` + `tokenizers`（Qwen2.5 tokenizer）跑，硬超时 3s。M2 可先只用 VAD 固定 min/max，turn-detector 作可选增强。

**Rust 结构**：`struct Endpointing{min_delay,max_delay,alpha}`（纯数据）；VAD EndOfSpeech 时 `tokio::spawn` 一个 EOT 任务：`sleep(min_delay)` 与 turn-detector 预测并行，按结果调整剩余 sleep，`select!` 可被新语音打断。

---

## B. 实时接力 + barge-in（秘书语音回应时用，M3+ 才需要）

我们的"秘书回应 + TTS"就是 LiveKit 的 LLM→TTS 环。等 M3（本地 TTS）接上再移植这套：
- **流式接力**：LLM 块 → 文本 channel → 按 `FlushSentinel`（句边界）切**句段** → **每段独立起 TTS 任务，首块即开播**；段落串行 `forward_generation` 播放。→ Rust：`mpsc<String|Flush>` + 每段一个 TTS 任务 + `select!` 播放/打断。
- **预生成**：中间转录就起 LLM 推理（不入队）；最终文本匹配则复用该 handle（省一轮）。
- **barge-in**：`SpeechHandle{ interrupt: oneshot, done: oneshot, allow_interruptions }`；用户出声 → `interrupt()`（截断 TTS + 取消在飞 LLM）。**假打断**：短暂出声先 `pause()` + 起 2s 计时，没续说就 `resume()`（不丢已说内容）。→ Rust：`tokio::sync::oneshot/watch` + `JoinHandle::abort()`。
- **调度器**：`BinaryHeap<SpeechHandle>` 优先队列 + `Notify`；`VoiceOrchestrator` 持状态机（AgentState: idle/listening/thinking/speaking）。

**并发原语映射**：`asyncio.Future→oneshot`；`Event→Notify`；`aio.Chan→mpsc(unbounded)`；`wait(FIRST_COMPLETED)→select!`；`Task→JoinHandle`；`heapq→BinaryHeap`；`call_later→spawn(sleep)`。

---

## C. 我们采纳 vs 跳过

**采纳**：VadStream/StreamAdapter 抽象、VAD 滞回参数、min/max 断句 + 语义 turn-detector（中文阈值现成）、流式接力+barge-in 状态机、并发原语映射。
**跳过/简化**：WebRTC/SFU/房间/多参与者/AEC（本地单用户）；LiveKit 的云 turn-detector-v1（用本地 ONNX mini 即可）。
**我们独有（LiveKit 没有）**：Stage3 topic 记忆——Stage2 整流后的节点异步喂给话题切分+摘要，作为 agent team 长期上下文（"秘书 vs 金鱼"分水岭）。

## M2 落地顺序（据此）
1. `audio-aura-asr` crate：`AudioChunk` + `trait Vad`（earshot）+ `trait StreamingAsr`（sherpa Zipformer-zh）+ `VadSegmenter`（供 SenseVoice 批式二次校准）。
2. 断句：`Endpointing{min,max}` + VAD 计时任务；turn-detector（`ort`+`tokenizers`，中文阈值 0.355）作可选第二步。
3. 音频源：`enum AudioSource { VisualScoutHttp, BrowserWsPcm }`；先浏览器 PCM 兜底。
4. 接入 daemon：Stage1 产出 chunk 落库 + `chunk_partial/chunk_final` SSE 事件；Stage2 仍用现有 RouterEngine。
