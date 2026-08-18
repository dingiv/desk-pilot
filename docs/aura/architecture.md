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
aura-core       (全栈)         ← 2026-08 合并了原 aura-core/aura-dcl/aura-store,
  ├─ recognizer (Stage1, 原 aura-asr executor, 2026-08-18 并入, feature `asr`) Silero VAD + 流式Zipformer
  │             + 批式ASR + WindowTracker 窗口边界 + AudioStore(PCM按id) + vad/buffer/scout 辅件
  │             + lib.rs 根部的边界契约 (VadSegment/VadWindow/Stage1Event)
  ├─ pipeline   (原 composer) Pipeline: Stage1→Stage2 (Stage2 独立线程)
  ├─ calibrator Stage2CalibratorImpl (窗口状态机) + Stage2Calibrator trait
  ├─ prompt     PromptBuilder (精简: 1句指令+few-shot+热词+纠偏+输出格式+多段联合)
  ├─ lib.rs     Calibrator (mistral.rs Qwen2.5-3B GGUF, impl LlmProvider)
  ├─ hub        Storage 总管: AudioArchive + TurnLog + recent ring
  ├─ archive    日期WAV落盘 + 热层回放
  ├─ tts        NoopTts 占位 (原 aura-tts 并入; 未来 Kokoro/Piper)
  └─ wav        WAV 读写
aura-agent      (Stage3+SDK)    能力 trait + HotwordManager + AddHotwordTool
                                + view(AuraStateView/AsrSegment 线协议) + AuraClient SDK
apps/audio-aura (daemon)      Pipeline + socket(8 routes + SPA fallback) + SSE双面 + Stage3规则触发器
crates/native                 napi shim (TS via VOICE_LOCAL_ROUTER)
```

**线程模型**：`aura-stage1-ingest`（scout→ring）→ `aura-pipeline`（std 线程跑 Stage1
consume loop）→ `aura-stage2`（LLM worker，mpsc 收 Batch/WindowEdge，partials 不被 LLM 卡住）
→ `aura-socket`（主线程 tokio，axum SSE）。详见 `stages.md`。

## 三阶段提交

| 阶段 | 职责 | crate | 抽象 |
|---|---|---|---|
| **Stage1** | 录音→VAD→段级流式会话+段级batch→窗口定稿（边界范式：VadSegment/VadWindow） | aura-core (`asr` feature) | Stage1Executor（发 Interim + Batch + WindowEdge） |
| **Stage2** | 窗口内多句联合整流（加标点/修同音字/英文规范/专有名词），无状态 | aura-core | Stage2Calibrator（calibrate_window / calibrate_final） |
| **Stage3** | 可选工具：热词 / 用户纠偏 | aura-agent | HotwordManager + CorrectionStore |

两阶段的完整流程与事件契约见 **`stages.md`**，设计沿革与 D1-D4 裁决见
`vad-segment-model.md`（2026-08-17 边界范式重构：PCM 由 AudioStore 按 id 持有、
batch 失败显式 Option、事件 append-only、Stage2 联合整流替代被删的 ContextWindow）。

**Stage2 简化**（2026-08）：只输出纯文本纠偏（`Decision`/`parse_decision`/`ContextWindow`
已随边界范式重构删除，校准直接返回 String）。PromptBuilder 精简（ROLE_TASK 1 句 +
OUTPUT 规则 + 多段联合输入 `new_multi`）。模型 **Qwen2.5-3B-Instruct**（~300ms）。

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

当前实跑配置（2026-08-17）：

```yaml
scout_addr: "127.0.0.1:7878"
port: 9091
model: "qwen2.5-3b-instruct-q4_k_m.gguf"  # Stage2 GGUF (llm_kind: local)
asr_backend: "qwen3-asr"        # sensevoice | whisper | qwen3-asr
asr_kind: "remote"              # local | remote（remote → qwen-asr-serve）
asr_endpoint: "http://127.0.0.1:8000"
hotwords: ["Rust", "Bevy", "贪吃蛇"]
log_level: "info"               # trace|debug|info|warn|error | EnvFilter 指令; RUST_LOG 优先
vad:
  min_silence: 1.0   # 切段灵敏度（低保响应）
  merge_gap: 2.5     # ★碎片合并窗口上界（"什么算一句话"的旋钮）
```

日志分级：**info 只出 final**（每定稿一句一条，含 batch/streaming/calibrated 三层文本）；
流式 partial（~0.5s/条）与纠偏碎片（~1s/条）为 **debug**——调管线时 `log_level: "debug"`。

优先级：CLI > aura.yaml > 内置默认（aura.json 仅作 loader 向下兼容 fallback）。
未来计划升级为 dp-models ModelSpec 结构化配置（`stage_asr:` / `stage_llm:` 嵌套段，
见 docs/dp-models.md）。

## 运行

```bash
# dev (assets/models + CARGO_MANIFEST_DIR)
CARGO_MANIFEST_DIR=$(pwd) cargo run -p aura-daemon --features asr,cuda -- 127.0.0.1:7879 -p 9091

# 或 Stage1→Stage2 bench
CARGO_MANIFEST_DIR=$(pwd) cargo run -p audio-aura-core --example stage12_live --features asr,cuda -- 127.0.0.1:7879
```

## 已验证

- crate 合并后全编译绿（aura-core 收编 dcl+store 后 ~1800 行；2026-08-18 再并入
  aura-asr/aura-tts 后 ~3250 行，43 单测 + ring_vad 集成测试）。
- Qwen2.5-3B 纠偏：~300ms/句，加标点+纠偏有效。
- dp-models remote：qwen-asr-serve + sglang 端到端跑通。
- 用户纠偏：POST /api/correct → CorrectionStore → Stage2 纠正段注入 → Web UI 编辑。
- Qwen3-ASR：strip_qwen3_markers 修标记泄漏（language Chinese<asr_text>）。
- 停顿碎片化：SegmentMerger（min_silence/merge_gap 解耦）+ edge_margin 边界扩展
  已实现并有单测（详见 real-world-speech-design.md §1/§1a）。
- 音频持久化：recordings/<日期>/*.wav + turns/<日期>.jsonl + 启动索引重建 +
  保留期清理（recordings_retention_days，默认 7 天）。
- 热词双层种子：boot 烘进流式 recognizer（beam bias）+ Stage2 共享 store；
  Stage3 运行时加词只进 LLM 层（下沉 ASR 是 M5）。

## 代码内遗留 TODO（整改时留意）

- `Stage1Executor::run` 静默阻塞线程、睡眠轮询 → 待异步非阻塞化（recognizer.rs）。
- `Stage1Config::new` 内嵌 IO（模型路径解析）→ 待拆出（recognizer.rs）。
- daemon 硬编码静态文件路径 `BASE` → 改用 FileLoader 机制（main.rs:582）。

## 未完成

见 `roadmap.md`。
