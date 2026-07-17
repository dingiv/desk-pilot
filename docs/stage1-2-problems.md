> ⚪ **已全部解决（2026-07-15）** —— Silero VAD、AudioRing 环形缓冲、流式两阶段 ASR、ContextWindow 上下文窗口均已落地。留作历史。**现状见 [architecture.md](architecture.md)。**

# 复盘补章（2026-07-17）：隐蔽丢音 —— 流式解码欠账 + VAD 强切

真机长句讲述测试再次出现"识别不完整、partial 与 final 对不上"。四个叠加根因（均已修）：

1. **流式解码欠账（核心）**：sherpa 的 `decode()` 是 *decode one step*（一步 ≈ 一个 chunk ~320ms），官方用法是 `while is_ready { decode }`；我们每 480ms 只调一次 → 每轮欠 ~160ms，长句下 partial 滞后滚雪球；EOS 时旧 session 直接丢弃，**积压未解码音频被无声扔掉**。修：`decode_and_result` 改 drain 循环；新增 `finalize_and_result`（`input_finished` + drain，冲出编码器尾块）。`is_ready` 门控同时天然防住了 fresh session 早解码的 GetFrames 崩溃 → 删掉 WARMUP_FRAMES hack。
2. **VAD 20s 强切腰斩**：sherpa `VoiceActivityDetector` 内部 buffer 超过 `max_speech_duration` 后自动切到急切模式（threshold=0.90 / min_silence=0.1s），在词中间强行 EOS（C++ 源码证实）。而 `min_silence=1.5s` 太长，讲述式句间自然停顿（0.5–1.2s）永远触发不了正常切分 → 全靠强切。修：min_silence 1.5→1.0s（自然停顿先切），max_speech 20→28s（只作兜底）。
3. **头部削字 / 短句整吞**：段起点回看仅 `2×window + min_speech`（试探期外余量 ~64ms），threshold 0.6 跨阈晚 → 首音节剪掉；`min_speech=0.5s` 把短命令整段丢弃。修：threshold→0.5（Silero 默认），min_speech→0.3s。
4. **问题被日志掩盖**：daemon 只打印 Stage2 整流文本（会删语气词、改写），分不清 ASR 丢字还是 LLM 改写。修：Final 行加打 批式/流式 原文。

验证：`streaming_asr_hot` 示例（500ms 轮询 100ms 喂入）partial 与音频进度同步、final 完整（旧代码下 5.6s 音频只解得 ~3.5s，final 必截断）；workspace 34 个测试套件全绿。

---

# Stage1+2 实时录音问题分析与解决方案

来源：真机录音测试反馈（2026-07-12）。四个问题：

1. 一阶段丢句子
2. 内存缓冲区缺失
3. 一阶段缺"手机输入法"式的流式纠偏
4. 二阶段缺窗口上下文管理

---

## 问题 1：一阶段识别不完整，会丢句子

**现象**：实时录音中有些话没被识别出来，整句丢。

**根因**：当前用**批式识别器**（SenseVoice）+ 外部 VAD。VAD 用固定 RMS 能量阈值判定"有没有在说话"——**轻声、尾音、语气词可能低于阈值，VAD 提前切分或整段丢弃**。另外 VAD 的 `min_speech_duration=50ms` 让极短词汇（"嗯""好"）也被追认为 speech-start 后直接丢弃。当程序在 ASR 推理时（~85ms），新的音频帧积压无缓冲，可能溢出丢帧。

**修理方案**：

### A. 换 VAD：用 sherpa-rs 自带的 Silero VAD（ONNX，比能量阈更准）

`audio-aura-asr` 已有 `VadSegmenter` + `EnergyVad`，但 sherpa-rs 直接提供了 `silero_vad` 模块（原汁原味 Silero）。这个 VAD 对轻声/hover/语气词的判别远超能量阈值。改动是 `AsrStream` 换用 `sherpa_rs::silero_vad::SileroVad`，VAD 参数照抄 livekit-agents 的 Silero 默认值（已在 `docs/livekit-port-notes.md` 记录）。

### B. 加音频环形缓冲区（见问题 2）：ASR 推理期间的帧先入队，不丢。

### C. VAD 参数微调

当前 `min_silence_duration=550ms` 合理；`min_speech_duration=50ms` 可收紧到 `300ms` 以减少碎段。

---

## 问题 2：需要内存缓冲区——音频环、Stage1 文本、Stage2 文本

**根因**：当前没有任何持久化缓冲。音频处理完就弃；ASR 文本一句一抛；Stage2 无上下文。这三个缓冲区是**三阶段提交架构的基础**（参见 `docs/chat.txt` Stage1 语音编码库 + Stage2 中间校准库）。

**方案：三个带容量限制的环形/截断缓冲区（均存内存）**

### 缓冲区 A：音频环形缓冲区（10 分钟）

```
容量：16kHz × 16bit × 1ch × 600s = 19,200,000 bytes ≈ 19.2 MB
结构：Arc<RwLock<RingBuffer<Vec<i16>>>> 或 deque<Vec<i16>> + 总样本计数
      可按 chunk（20ms=320 samples）为单位存储，方便回放/重识别
作用：
  - 丢句时从环里回溯音频片段重新做 ASR（user 说"刚才那句不对"）
  - 支持流式纠偏（问题 3 的 back-look window）
  - 未来用于 Stage1 原始录音溯源（chat.txt 的"重放与溯源"）
```

### 缓冲区 B：Stage1 文本缓冲区（3MB）

```
容量：~3MB UTF-8，约 ~1,500,000 个中文字符
结构：Vec<UtteranceRecord>（每句含：时间戳、raw_text（多 ASR 候选）、最终采纳文本）
作用：
  - 流式纠偏：新句来时可以"回头看"修改前面的 text（问题 3）
  - Stage2 上下文的数据源
  - 落入磁盘（audio-aura-store SQLite voice_chunks 表）的同时在内存中驻留
```

### 缓冲区 C：Stage2 校准文本缓冲区（10MB）

```
容量：~10MB UTF-8，约 ~5,000,000 个中文字符
结构：Vec<CalibratedNode>（每节点含 linked_chunks、校准文本、topic_id）
作用：
  - Stage2 窗口上下文（问题 4）
  - Stage3 topic 记忆的输入
  - 秘书的"工作记忆"(working memory)
```

**数据结构统一放到 `audio-aura-core` 或新 crate `audio-aura-buffer`（还没决定），但先给出 trait 设计**：

```rust
trait AudioRingBuffer {
    fn push(&mut self, chunk: AudioChunk);
    fn slice(&self, range: Range<Duration>) -> Vec<i16>; // 回溯某段时间的 PCM
    fn len_samples(&self) -> usize;
}

struct Stage1Buffer {
    utterances: Vec<UtteranceEntry>, // ordered by time, newest at end
    total_bytes: usize,
    max_bytes: usize,                // 3_000_000
}

struct Stage2Buffer {
    nodes: Vec<CalibratedNode>,
    total_bytes: usize,
    max_bytes: usize,                // 10_000_000
}
```

---

## 问题 3：一阶段缺少流式纠偏能力（像手机输入法的"后文修正前文"）

**现象**：手机上的语音输入法在用户继续说话时，会神奇地把**前文已经打出来的错字自动修正**（例如"公式"→"公司"，因为后文说了"下个月的财报"）。当前我们用的是**批式 SenseVoice + 一次性输出**，不具备这种能力。

**根因**：这是**流式(streaming) ASR 与批式(batch) ASR 的核心差异**。

- 批式 ASR（SenseVoice）：输入整段音频 → 一次性输出文本。无中间状态、无法回溯修改。当前 `SherpaAsr(recognize: batch)` 即为此。
- 流式 ASR（Zipformer / Paraformer streaming）：持续输入 PCM 帧 → 持续输出**中间文本**(partial) + **最终文本**(final)。CTC/Transducer 解码器内部维护一个**束搜索(beam search)**——当更多音频到达时，束搜索中的低概率路径被剪枝、高概率路径晋升，从而**自动修正前文**。

**手机输入法的流式纠偏本质** = 流式 CTC-Attention 解码器的**束搜索动态重构 + Look-ahead Window + 后端 N-gram/RoBERTa 语言模型实时回调**。

### 方案：上流式 ASR（sherpa-onnx `OnlineRecognizer`）

sherpa-rs 有 `zipformer` 模块（`sherpa-rs/src/zipformer.rs`），对应 sherpa-onnx 的**流式 Zipformer-zh transducer**（纯流式、延迟 <300ms，首词即出中间文本）。

```rust
// 当前（批式）:
let text = asr.recognize(&pcm, 16000)?;   // 一次性，无中间结果、无纠偏

// 改为流式:
let mut recognizer = OnlineRecognizer::new(zipformer_config)?;
for frame in audio_chunks {
    recognizer.accept_waveform(frame_samples, 16000);
    while let Some(result) = recognizer.decode() {
        // result.text = 当前中间文本（束搜索持续更新）
        // 后续帧会改变前面帧的文本 —— 这就是"手机输入法纠偏"
        emit(SpeechEvent { kind: Interim, text: result.text });
    }
}
// 输入结束后 finalize:
let final_text = recognizer.input_finished_and_decode()?;  // 最终文本
emit(SpeechEvent { kind: Final, text: final_text });
```

**实现要点**：
- 流式 Zipformer-zh 模型约 ~80-120MB（与 SenseVoice int8 同量级）
- 首词延迟 300-600ms，中间结果 50-100ms 刷新
- **束搜索会在更多音频到达后自动修正前文**（这恰恰是用户要的"手机输入法效果"）
- 与 Stage1 音频环配合：用户说"上一句不对"，可以从环里取出前 30s 的 PCM 重喂流式 ASR

**替代/补充方案**（如果流式 Zipformer 模型太大或要更准）：**二次校准（Two-pass）**——流式 Zipformer 做第一遍（快、有中间纠偏），端点后用批量 SenseVoice 做第二遍（高精度覆盖）。这跟 chat.txt 中 Stage2 的二次校验一脉相承。

### 修正后的 Stage1 伪代码

```rust
// 每 20ms 帧:
audio_ring.push(frame);
streaming_asr.push_frame(frame);
// 检查 ASR 中间结果（可能已修正前文）:
if let Some(interim) = streaming_asr.poll_interim() {
    // 更新 Stage1 文本缓冲区中的上一句文本（或追加）
    stage1_buffer.update_last_or_append(interim);
}
// VAD 判定端点:
if vad.is_endpoint() {
    let final_text = streaming_asr.finalize();
    // 如果有二次校准(SenseVoice batch)，在终段跑它，覆盖 final_text
    let calibrated = two_pass_recognize(&audio_ring, &vad_segment);
    stage1_buffer.commit(calibrated);
    stage2_calibrate(&stage1_buffer.recent(6));  // 上下文化（问题 4）
}
```

---

## 问题 4：二阶段需要窗口上下文管理

**现象**：当前 `stage12_live.rs` 的 Stage2 调用是无上下文的：

```rust
router.route_blocking(&raw, None)  // None = 完全没有上下文 ❌
```

**根因**：Qwen 路由器一句一句独立推理，不知道上一句聊了什么。导致：

- 同音字无法根据上下文纠正（"公司/公式"、"开饭/开放"）
- 意图判定无语境——可能把"接着说第二点"判成闲聊
- 无法合并跨句意群（"把刚才那个论点改成 X"）
- 缺少 topic/agent 的 working memory

**方案**：Stage2 每次推理时带**滑窗上下文**——拼接最近 N 句的整流文本作为 prompt 的一部分。这就是 `audio-aura/src/service.ts` 里 `RECENT_CONTEXT=6` 做的事（TS 版已有），Rust 版还没接。

### 滑动窗口上下文设计

```rust
fn build_context(buf: &Stage2Buffer, window_size: usize) -> String {
    // 取最近 window_size 个校准节点，拼接为：
    // "最近对话：\n1. <校准文本1>\n2. <校准文本2>\n...\n\n用户刚说：<raw_text> /no_think"
    let recent: Vec<&str> = buf.nodes.iter().rev().take(window_size).map(|n| n.calibrated_text.as_str()).collect();
    let ctx = recent.into_iter().rev().enumerate()
        .map(|(i, t)| format!("{}. {}", i + 1, t))
        .collect::<Vec<_>>()
        .join("\n");
    if ctx.is_empty() { ctx } else { format!("最近对话：\n{}\n\n用户刚说：{} /no_think", ctx, raw_text) }
}
```

### 合并到路由调用

```rust
// 阶段 2 推理（router 提示词已含上下文窗口）
let ctx = build_context(&stage2_buffer, 6);
let out = router.route_blocking(&raw, Some(&ctx))?;
// parse_decision 不变
```

### Stage2 提示词也要同步扩展

```
SYSTEM = "你是语音秘书的本地大脑。... 你需要根据【最近N句对话上下文】来判断当前句：
- 若当前句是对上一句的修正或补充（如'不对，应该是X'），将其合并到上一句的校准文本中。
- 若开启了新话题，单独成句并判定为 task。
- 修正同音字时要利用上下文语义（如'上午九点去工元'→'公园'因为上文提到'周末计划'）。
..."
```

### 效果

- 上下文解决同音字（`开饭→开放`、`工元→公园`）
- 合并跨句意群（`把这个删掉` + `改成今天下午` → 一步修正）
- 意图判定更准（有对话背景）
- 为 Stage3 topic 记忆积累上下文

---

## 总结——修复优先级与改动清单

| 问题 | 改动 | 优先级 | 涉及文件 |
|---|---|---|---|
| 1. 丢句子 | VAD 换 Silero（sherpa-rs `silero_vad`）+ 音频环缓冲 | P0（先修） | `audio-aura-asr` 加 SileroVad，`audio-aura-core` 加 AudioRingBuffer |
| 2. 缓冲区 | 三个内存缓冲区（音频环/Stage1 文本/Stage2 校准文本）| P0（先建） | 新 `audio-aura-buffer` crate 或在 `audio-aura-core` 内加 `buffer` 模块 |
| 3. 流式纠偏 | 流式 ASR（sherpa-onnx Zipformer OnlineRecognizer）替代/补充批式 SenseVoice | P1（次修）| `audio-aura-asr` 加 `ZipformerStreamingAsr` 实现 `Asr` trait |
| 4. 窗口上下文 | Stage2 推理带滑窗上下文（`route_blocking(raw, Some(ctx))`） | P0（先修）| `audio-aura-router` 提示词扩展 + `audio-aura-core` `stage12_live` 调用改 |
