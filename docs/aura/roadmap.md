# 路线图（2026-08 更新）

> 对照 `architecture.md` 现状。北极星：[[ai-secretary-north-star]]。

## ✅ 已完成

- [x] crate 合并（aura-core + aura-dcl + aura-store + aura-asr + aura-tts → 一个 aura-core）
- [x] **边界范式**（2026-08-17）：VadSegment/VadWindow 一等实体 + append-only 事件
  （Batch/WindowEdge），替代旧 Utterance/MergeBatch"就地修改"契约；PCM 由 AudioStore
  按 id 持有，batch 失败显式 Option。
- [x] **5 事件数据面协议**（2026-08-18）：stream_fragment / batch_segment / batch_window /
  segment_calibration / window_calibration + correction；aura-core/agent/daemon/swift-ime/
  geek-familiar 已切换，devtools 待迁。
- [x] dp-models 通用模型提供库：ModelProvider 伞形 trait + Asr/Llm/Vlm 能力 trait +
  MistralLlm（mistralrs 迁入）/ OnnxAsr / Http*（远程）。aura-core 默认构建不再编译
  GPU/LLM 重依赖。
- [x] daemon asr/llm local/remote/disable 配置切换（aura.yaml，未知键拒绝）
- [x] **能量门**（2026-08-19）：空闲跳过 VAD/流式 NN + 修 x-asr 静音幻觉 + 挂机 PCM 泄漏
- [x] 流式引擎第二选项 **x-asr**（自带标点；tokens.txt 官方两列格式）
- [x] 模型瘦身：assets/models 26GB → 12GB（删孤儿；本地批式只留 sensevoice，whisper/
  qwen3-asr 本地分支已删）
- [x] Stage2 简化：纯文本输出、PromptBuilder 精简、`llm.input` 纠偏源（batch/stream/both）
- [x] ContextWindow 禁用（3B 复读问题）
- [x] 用户纠偏：POST /api/correct → Stage2 corrections 段注入 + Web UI 编辑
- [x] edge_margin 段边界扩展（0.3s）——修句首/尾掉字
- [x] 停顿碎片化解决：min_silence(切段) 与 merge_gap(合并窗口) 解耦 + 窗口级 batch 重跑
- [x] 音频持久化：recordings WAV（按日期）+ turns jsonl + 保留期清理（retention_days）
- [x] aura.json → aura.yaml（支持注释）
- [x] assets/ 统一（models + sherpa + cudnn + lib）

## 🔴 近期

| # | 事项 | 价值 | 说明 |
|---|---|---|---|
| R1 | **自适应 merge_gap** | 中 | 碎片化主体已被 SegmentMerger 解决；剩余价值=按场景自适应"一句"的窗口（命令式调小快定稿 / 长句调大） |
| R4 | **口误自纠检测**（正则 "X 不对 Y" → Y） | 中 | 零 LLM 开销的确定性预处理 |
| R5 | **Stage1 run() 异步化（残余：batch 移出消费线程）** | 中 | ~~睡眠轮询~~ 已除（2026-08-18：ring 挂 Condvar，无帧时挂起等 ingest notify，仅真实截止时间唤醒——settle 到点/停滞看门狗/断流喂静音；无截止时间则无限期挂起，**空闲零唤醒、零心跳**，diag 只在有活动时打印）。仍待：**batch 调用移出消费线程**——EOS/窗口定稿时的段级+窗口级 batch 是消费循环内同步调用，远程 ASR（mloader qwen3-asr）实测 ~3.5s/次，期间流式 partial 与 VAD 全停；真实语速下 EOS 间隔 >1s 尚可接受，但批量重放/长窗口时有感知延迟（2026-08-17 e2e 实测记录，见 stages.md）。副作用实证：batch 阻塞期间 ring 积压，解除后追赶音频被压缩处理——**墙钟 gap 被压扁导致过度并窗**（10x 重放下 4s 间隔压成 0.4s 实测复现）。临时缓解：`asr.backend: disable` 纯流式模式直接跳过全部 batch 调用 |
| R6 | **Stage1Config::new IO 拆分** | 低 | 构造函数内嵌模型路径解析 IO，拆成独立函数（recognizer.rs TODO） |
| R7 | **daemon 静态路径去硬编码** | 低 | `BASE` 常量 → FileLoader 机制（main.rs TODO） |

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
