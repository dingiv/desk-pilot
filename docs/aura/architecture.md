# aura 架构（as-built 2026-08）

> 现状权威文档。代码为准。北极星：[[ai-secretary-north-star]]。

## 定位：语音助手前端 + 中间守护进程

aura 是 **desk-pilot** 的语音子系统——"耳朵 + 整流 + 意图"。下接 omni-scout（录音源），
上接 geek-familiar（秘书 UI + agent 调度）。

```
geek-familiar (秘书 UI + agent)
      │  socket
      ▼
aura-daemon (三阶段管线 + HTTP socket)
      │  HTTP (omni-scout /audio) 或 mock-audio
      ▼
omni-scout (PipeWire 真麦 / mock wav)
```

## crate 拓扑（无环，2026-08 合并后）

```
shared          (FileLoader 叶子) — dev/prod 路径解析
dp-models       (跨子系统叶子) — AsrProvider/LlmProvider/VlmProvider trait + Http* remote
aura-asr        (Stage1 叶子)   VAD + 流式Zipformer + 批式ASR + Stage1Executor
aura-core       (Stage2+组装+存储) ← 2026-08 合并了原 aura-core/aura-dcl/aura-store
  ├─ composer   Pipeline: Stage1→Stage2 (Stage2 独立线程)
  ├─ calibrator Stage2CalibratorImpl + Stage2Calibrator trait
  ├─ prompt     PromptBuilder (精简: 1句指令+few-shot+热词+纠偏+输出格式)
  ├─ context    ContextWindow (已禁用 — 3B 模型会复读; 7B+ 再开)
  ├─ decision   Decision/parse_decision (简化: 纯文本输出, 无JSON)
  ├─ lib.rs     Calibrator (mistral.rs Qwen2.5-3B GGUF, impl LlmProvider)
  ├─ hub        Storage 总管: AudioArchive + TurnLog + recent ring
  ├─ archive    日期WAV落盘 + 热层回放
  └─ wav        WAV 读写
aura-agent      (Stage3)      能力 trait + HotwordManager + AddHotwordTool
aura-tts        (占位)        NoopTts (未来 Kokoro/Piper)
apps/audio-aura (daemon)      Pipeline + socket(18 routes) + SSE + Stage3规则触发器
crates/native                 napi shim (TS via VOICE_LOCAL_ROUTER)
```

## 三阶段提交

| 阶段 | 职责 | crate | 抽象 |
|---|---|---|---|
| **Stage1** | 录音→VAD→两阶段ASR（流式Zipformer partial + 批式ASR final） | aura-asr | Stage1Executor（发 Interim/Final） |
| **Stage2** | 口语纠偏（加标点/去语气词/修同音字） | aura-core | Stage2Calibrator（calibrate(Utterance)→Decision） |
| **Stage3** | 可选工具：热词 / 用户纠偏 | aura-agent | HotwordManager + CorrectionStore |

**Stage2 简化**（2026-08）：去掉了 JSON 输出（intent/reply/task），**只输出纯文本纠偏**。
PromptBuilder 精简（ROLE_TASK 1 句 + few-shot + 热词 + 纠正段 + OUTPUT）。
模型从 Qwen3-1.7B（thinking）换成 **Qwen2.5-3B-Instruct**（指令模型，~300ms，质量更好）。

## dp-models Provider 抽象（2026-08 新增）

跨子系统（aura/visual-rover）统一 local/remote 模型抽象：

```
dp-models/
├── trait AsrProvider  { recognize(pcm, sr) -> text }
├── trait LlmProvider  { complete(system, user) -> text }
├── trait VlmProvider  { complete(system, user, image) -> text }
├── HttpAsr/HttpLlm/HttpVlm  (OpenAI 兼容 remote, reqwest::blocking)
└── ProviderKind: Local | Remote { endpoint }
```

aura 已接入：executor `batch_asr: Arc<dyn AsrProvider>`（local OnnxAsr / remote HttpAsr）；
Pipeline `s2: Box<dyn Stage2Calibrator>`；daemon `asr_kind`/`llm_kind` 配置切换。

## 双运行时（ONNX + HF）

进程内两个隔离运行时，各管各的、只通过文本交互：
- **ONNX 侧**（sherpa-onnx 官方 crate）：VAD(Silero) + ASR(SenseVoice/Whisper/Qwen3-ASR)。
  sherpa .so 在 `assets/lib/`（绝对路径软链，RUNPATH `$ORIGIN` 自定位）。
- **HF 侧**（mistral.rs/candle，GPU sm_120）：Qwen2.5-3B-Instruct 纠偏（Stage2）。

## 配置（aura.yaml，YAML 支持注释）

```yaml
scout_addr: "127.0.0.1:7878"
port: 9091
model: "qwen2.5-3b-instruct-q4_k_m.gguf"  # Stage2 GGUF
asr_backend: "qwen3-asr"        # sensevoice | whisper | qwen3-asr
asr_kind: "local"               # local | remote
hotwords: ["Rust", "Bevy"]
```

## 运行

```bash
# dev (assets/models + CARGO_MANIFEST_DIR)
CARGO_MANIFEST_DIR=$(pwd) cargo run -p aura-daemon --features asr,cuda -- 127.0.0.1:7879 -p 9091

# 或 Stage1→Stage2 bench
CARGO_MANIFEST_DIR=$(pwd) cargo run -p audio-aura-core --example stage12_live --features asr,cuda -- 127.0.0.1:7879
```

## 已验证

- crate 合并后全编译绿（aura-core ~2000 行, 合并 dcl+store）。
- Qwen2.5-3B 纠偏：~300ms/句，加标点+去语气词+纠偏有效。
- dp-models remote：qwen-asr-serve + sglang 端到端跑通。
- 用户纠偏：POST /api/correct → CorrectionStore → Stage2 纠正段注入 → Web UI 编辑。
- Qwen3-ASR：strip_qwen3_markers 修标记泄漏（language Chinese<asr_text>）。

## 未完成

见 `roadmap.md`。
