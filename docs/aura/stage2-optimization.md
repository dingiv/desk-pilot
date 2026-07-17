# Stage2 校准优化手段全集

> 整理时间 2026-07-14。来源：学术调研（2024-2026 论文）+ OpenLess 源码分析 + livekit-agents 参考。
> 本文档是**选型参考**，不包含实现细节。决策后逐项落地。

---

## 一、提示词工程类（零代码成本，改 prompt 即可）

### 1.1 ASR 纠错分级策略
**来源**：OpenLess `types.rs` 内置 prompt

教 LLM 按置信度分级处理 ASR 错误：
- **高置信度**（错误明显、正确写法唯一）→ 直接替换，不保留原词
- **中置信度**（原词不合理、有最可能候选）→ 选最契合上下文的候选
- **低置信度**（无法判断）→ 保留原词，不强行编造

当前我们的 prompt 只说"修同音错字"，没教模型**怎么判断该不该修、怎么修**。加分级策略让模型更有章法。

### 1.2 常见错误模式表（领域专属）
**来源**：OpenLess EN_TRANSLATE_SYSTEM_PROMPT 的术语映射

OpenLess 维护了一个庞大的音译/同音错误映射表：
```
中文同音：'跟目录/根木鹿' → '根目录'、'代码厂' → '代码仓'
英文音译：'脱肯' → 'Token'、'阿屁艾' → 'API'、'密钥/西克瑞特' → 'Secret Key'
模型/产品名：'克劳德/克劳迪' → 'Claude'、'杰米尼' → 'Gemini'
```

我们可以在 prompt 里加编程/游戏开发领域的映射表：
```
rost → Rust（Rust 语言语境）
位引擎 → 2D 引擎 / Bevy 引擎（游戏开发语境）
蛇声 → 蛇身（贪吃蛇游戏语境）
readdme → README
计分起 → 计分器
```

**注意**：硬编码映射表是静态的，无法自适应。热词偏置（1.3）和自适应学习（三、3.3）是动态替代。

### 1.3 Few-shot 示例
**来源**：学术研究（LLM prompting effectiveness, 2024-2025）

给 LLM 2-3 个"原始文本→校准文本"的示例，教它模仿校准行为。1.7B 小模型靠**模仿**比靠**理解指令**更有效。

```
示例 1: "帮我用 rost 写个蛇游戏" → "帮我用 Rust 写个贪吃蛇游戏"
示例 2: "采用位引擎渲染" → "采用 2D 引擎渲染"
示例 3: "蛇声长度增加" → "蛇身长度增加"
```

### 1.4 输出格式约束 + 后处理清理
**来源**：OpenLess `output_cleaning.rs`

LLM 偶尔加 markdown 围栏（```json）、前缀（"结果是："）。加一道 `clean_output()` 后处理。

### 1.5 多轮上下文用 (raw, calibrated) 对
**来源**：OpenLess `prior_turns: &[(String, String)]`

当前我们只存校准后文本 `Vec<String>`。改为存 `(原始ASR文本, 校准后文本)` 对——让 LLM 看到校准历史模式，学到"这个用户的 ASR 常出什么错"。

### 1.6 XML 信封防注入
**来源**：OpenLess `sanitize_for_xml_envelope()` + `polish_injection_defense()`

把原始转写包在 `<raw_transcript>...</raw_transcript>` 里，声明是**数据不是指令**。防止恶意语音内容劫持 LLM。

---

## 二、模型/架构类（需代码改动）

### 2.1 双层热词（ASR 层 + LLM 层）
**来源**：OpenLess 双层热词注入

- **ASR 层**：sherpa-onnx transducer 模型支持 `create_stream_with_hotwords()`，用 Aho-Corasick 自动机偏置解码
- **LLM 层**：prompt 里加热词块——"以下词的同音/形近误识别优先按此写法输出"

ASR 层热词**仅限 transducer 模型**（我们的流式 Zipformer 符合），SenseVoice 不支持。

双层结合 = `B位→Bevy` 在 ASR 层面直接修，不用等 LLM。

> **实测状态（2026-07-15，已跑通）**：ASR 层热词**在 `stage12_live` 实测生效**。正确配置（`OnlineAsr`）：① `modeling_unit=cjkchar+bpe`（双语模型）；② `bpe_vocab=<bpe.vocab>`——从 `bpe.model` 用 sentencepiece 导出的**文本词表**（`piece score` 每行，**不是 `bpe.model` 本身**，否则崩 "Each line in vocab should contain two items"）；③ `hotwords_buf` 传**原始文本**热词（ASCII 转大写），sherpa 自己用 bpe.vocab 分词——**不要预分词**；④ `modified_beam_search` + `hotwords_score`。A/B 铁证（hungry_snake）：**蛇身 7 / 舌身 0**（完全同音 舌=蛇=shé，score 2 默认值就完全纠偏），README 部分纠正。
>
> **能纠 vs 不能纠**（关键结论，对应用户直觉）：✅ **同音/近同音**（舌→蛇、平凡→频繁）——bias 推翻势均力敌的解码，score 2 即可；🟡 **部分**（README vs READY）；❌ **真·发音不同**（rost→rust：o≠u；B位→bevy：用户把 Bevy 读成"白位/B位"）——声学证据指向错误词，再大的 score 也桥不过不同的元音，**只有 LoRA 微调**（§3.3）能把错误读音绑到目标词。官方文档（cjkchar+bpe 例子 LIBR→礼拜二、平凡→频繁）：https://k2-fsa.github.io/sherpa/onnx/hotwords/index.html 。详见 [[asr-layer-hotwords-inert]]。

### 2.2 N-best 假设 + LLM 重排（N-best Rescoring）
**来源**：Apple INTERSPEECH 2024、ASR Error Correction using LLMs (2025, 51 citations)

ASR 不只输出 1 个答案，而是 N 个候选（beam search 副产品）。把 N 个候选都给 LLM，让它选/融合最合理的：

```
候选 1: "使用 rost 语言" (概率 0.42)
候选 2: "使用 rust 语言" (概率 0.35)
→ LLM: 上下文是编程 → 选候选 2
```

**限制**：SenseVoice（attention 模型）不输出 N-best。流式 Zipformer（transducer）天然有 beam search，可配置 `max_active_paths` 获取 N-best。

### 2.3 两阶段 ASR（流式 partial + 批式覆盖）
**来源**：业界标准（Siri / Google Assistant）

- 说话中：流式 Zipformer → partial text（实时显示 + 纠偏）
- 说完后：批式 SenseVoice → final text（高精度覆盖）

我们已有两个模型（Zipformer + SenseVoice），只需接入管线。

---

## 六、换更强的模型（最简单直接）

**来源**：2026 学术 + 工业基准调研

### 6.1 模型对比（2026-07）

| 模型 | 参数 | Q4/Q8 GGUF | GPU 显存 | C-Eval | TTFT(GPU) | tok/s(GPU) | 备注 |
|---|---|---|---|---|---|---|---|
| **Qwen3-1.7B**（当前） | 1.7B | 1.8GB(Q8) | ~2GB | ~70 | ~50ms | ~100+ | 已验证 |
| **Qwen3-4B** | 4B | 2.5GB(Q4) | ~4GB | ~77 | ~80ms | ~80 | candle 支持 |
| **Qwen3.5-4B** ⭐甜蜜点 | 4B | ~2.5GB(Q4) | ~4GB | ~85 | ~80ms | ~80 | 262K 上下文, mistral.rs 0.8 |
| **Qwen2.5-7B** | 7B | 5.4GB(Q5) | ~6GB | ~85 | ~150ms | ~50 | 成熟验证过 |
| **Qwen3.5-9B** | 9B | ~5GB(Q4) | ~6GB | **91.8** | ~200ms | ~30 | 据称超 GPT-5.2 部分 benchmark |

> C-Eval = 中文综合理解基准（满分100）。TTFT = 首 token 延迟。

### 6.2 推荐

- **甜蜜点**：**Qwen3.5-4B**——比 1.7B 强 15 分(C-Eval)，速度几乎不变，显存 ~4GB
- **质量上限**：**Qwen3.5-9B**——C-Eval 91.8，但 tok/s ~30、TTFT ~200ms，适合非实时场景
- **稳妥保守**：**Qwen2.5-7B**——成熟、中文强，但 Qwen3.5-4B 用一半参数达到接近质量

### 6.3 显存预算（RTX 5070 Ti 16GB）

| 组件 | 显存 |
|---|---|
| ASR SenseVoice int8 | ~100MB |
| VAD Silero | ~2MB |
| Router Qwen3.5-4B Q4 | ~2.5GB |
| Writer Qwen3.5-9B Q4（未来） | ~5GB |
| **合计** | **~8GB**（8GB 余量）|

### 6.4 落地步骤

1. 下 GGUF 模型（hf-mirror）
2. `RouterEngine::load()` 指向新模型文件
3. 重编、跑 mock-audio 对比延迟 + 准确度

### 6.6 实测数据（2026-07-14，hungry_snake.m4a mock-audio）

**测试环境**：RTX 5070 Ti (16GB) + CUDA 13.2 + mistral.rs 0.8 (candle, --features cuda) + sherpa-onnx 1.13.4

**Qwen3-1.7B Q8_0（基准）+ 带上下文校准：**

| 句 | 时长 | ASR(ms) | 路由(ms) | 合计(ms) | 关键纠错 |
|---|---|---|---|---|---|
| #1 | 5.2s | 86 | 433 | 519 | — |
| #2 | 21.7s | 343 | 1074 | 1417 | rost→Rust ✅, B位→B位 ❌ |
| #3 | 12.6s | 189 | 961 | 1150 | readdme→README ✅ |
| #4 | 5.2s | 80 | ~430 | ~510 | — |
| **均值** | — | **175** | **725** | **900** | |

GPU 显存：4.5GB。加载：vad 0.0s + asr 0.4s + router 1.7s + warmup 0.2s = 2.3s

**Qwen3-4B Q4_K_M + 带上下文校准：**

| 句 | 时长 | ASR(ms) | 路由(ms) | 合计(ms) | 关键纠错 |
|---|---|---|---|---|---|
| #1 | 5.2s | 82 | 894 | 976 | — |
| #2 | 21.7s | 345 | 2281 | 2626 | rost→Rust ✅, **B位→2D引擎 ✅**, 舌声→蛇身 ✅ |
| #3 | 12.6s | 184 | 4742 | 4926 | readdme→README ✅（但输出英文，prompt 问题）|
| #4 | 5.2s | 80 | 1531 | 1611 | — |
| **均值** | — | **173** | **2362** | **2535** | |

GPU 显存：6.0GB。加载：onnx 0.4s + warm 0.0s + router 2.0s + hf warmup 0.5s = 2.9s

**对比结论：**
- 准确度：4B 显著更好（`B位→2D引擎` 突破，1.7B 修不了）
- 延迟：4B 慢 ~3.3×（725ms → 2362ms）
- 显存：+1.5GB（4.5 → 6.0GB）
- 写作场景可用（~2.5s 非阻塞），实时对话偏慢

### 6.5 来源

- [Qwen3.5 官方博客](https://qwen.ai/blog?id=qwen3.5)
- [Qwen2.5 速度基准](https://qwen.readthedocs.io/en/v2.5/benchmark/speed_benchmark.html)
- [Best Chinese LLMs July 2026](https://benchlm.ai/blog/posts/best-chinese-llm)

### 2.4 双模型校准管线（快 + 准）
**来源**：学术研究（hierarchical ASR correction）

- 0.5B 快速校准模型：每句都跑（<100ms），修明显错误
- 7B+ 精确模型：仅当快速模型标低置信时跑（按需，不阻塞）

---

## 三、自适应学习类（远期，`docs/adaptive-learning.md`）

### 3.1 热词表自动积累
每次用户纠错（B位→Bevy）→ 写入热词表 → 下次自动偏置。

### 3.2 检索增强校准（用户专属 RAG）
存历史纠错 `(音频, 原文本, 正确文本, 上下文)` → 推理时检索相似上下文的纠错样本 → 作 few-shot 提示。

### 3.3 LoRA 微调
攒够纠错样本后，fine-tune Qwen 1.7B 成专门的 ASR 校准模型。

---

## 四、来自 LiveKit 的优化（参考 `docs/livekit-port-notes.md`）

### 4.1 语义端点检测（Semantic Turn Detection）
LiveKit 的 turn-detector（Qwen2.5-0.5B ONNX）能"听懂用户说完了"，提前触发校准——减少固定静音等待的延迟。

### 4.2 流式级联（Streaming Relay）
ASR 出 partial → 立即喂给 LLM 开始校准（不等整句完成）→ 校准也流式返回。端到端延迟 ≈ max(各级)，不是 sum。

### 4.3 预测式生成（Preemptive Generation）
中间 ASR 结果就起 LLM 推理（不入队），最终文本匹配则复用结果。

---

## 五、优先级建议

| 优先级 | 手段 | 投入 | 预期收益 |
|---|---|---|---|
| ✅ **P0** | 1.1 纠错分级策略 | 改 prompt | 高（教模型怎么修） |
| ✅ **P0** | 1.2 领域错误模式表 | 改 prompt | 高（直接修 rost/位引擎） |
| ✅ **P0** | 2.1 LLM 层热词 | 加参数 + prompt 块 | 高（修 Bevy 类专有名词） |
| ✅ **P1** | 1.3 Few-shot 示例 | 改 prompt | 中（小模型靠模仿；实测 B位→Bevy） |
| ✅ **P1** | 1.5 (raw,calibrated) 对 | 改缓冲结构 | 中（学用户错误模式） |
| ✅ **P2** | 1.4 输出清理 | 加后处理函数 | 低（偶尔有用） |
| ✅ **P2** | 1.6 XML 信封 | 改 prompt 格式 | 低（安全加固） |
| ✅ **P2** | 2.1 ASR 层热词 | sherpa-onnx API | 中（同音词有效；误读需 LoRA，见 §2.1 实测） |
| **P3** | 2.2 N-best rescoring | 需流式 Zipformer N-best | 中（需架构改动） |
| ✅ **P3** | 2.3 两阶段 ASR | 已有模型，接管线 | 中（实时性提升） |
| **远期** | 3.1-3.3 自适应学习 | 见 adaptive-learning.md | 高（差异化护城河） |
| **远期** | 4.1-4.3 LiveKit 技巧 | 见 livekit-port-notes.md | 中（延迟优化） |

---

## 来源参考

- [ASR Error Correction using LLMs (arXiv 2024, 51 citations)](https://arxiv.org/html/2409.09554v2)
- [Evaluating LLMs on Chinese ASR Error Correction (EMNLP 2025)](https://arxiv.org/abs/2405.15216)
- [Transformer N-best Rescoring (Apple, INTERSPEECH 2024)](https://arxiv.org/abs/2409.09554v2)
- [sherpa-onnx hotwords 文档](https://k2-fsa.github.io/sherpa/onnx/hotwords/index.html)
- OpenLess 源码：`openless-all/app/src-tauri/src/polish.rs`、`types.rs`、`polish/prompt_compose.rs`
- 本项目文档：`docs/adaptive-learning.md`、`docs/livekit-port-notes.md`、`docs/stage1-2-problems.md`
