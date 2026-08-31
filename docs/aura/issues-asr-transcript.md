# ASR 显示两类 bug 调查与 transcript 归位(round11)

> 创建: 2026-08-31。前端(s swift-ime / ime-core)与 aura daemon 的实时语音
> 交互出现两类显示 bug;调查后确认**三个独立缺陷**,其中两个是根因、一个是
> 次生。本轮一并修掉两个根因,并把语音事件的折叠细节从 ime-core 归位到
> aura-agent(上层不再关注 AsrEvent 细节)。

## 一、症状

1. **语音内容显示不全**:流式/定稿文本偶发缺字、出现 `�`(U+FFFD)。
2. **上一个语音结果被新句子覆盖置换**:说完一句(小停顿)继续说下一句,
   前一句本该留在候选里,却被新句子顶掉、消失数秒。

## 二、调查结论(证据链)

### Bug 1「显示不全」= SSE 客户端逐 chunk 做 UTF-8 lossy 解码(实锤)

`aura-agent/src/client.rs:221`(`sse_data` 的读循环):

```rust
buf.push_str(&String::from_utf8_lossy(&chunk));   // ← 每 TCP chunk 解码一次
```

中文识别文本是三字节 UTF-8;TCP 分片把它拦腰斩断时,`from_utf8_lossy`
把半个字符变成 U+FFFD。注释声称 "SSE frames are ASCII (JSON pings /
keep-alive comments)" —— 写注释时只考虑了控制面;数据面 `/api/asr_stream`
全是中文。随机、间歇、随分片时机 → 恰好符合"有时显示不全"。

**修法(S1)**:缓冲原始 `Vec<u8>`,只在 `\n\n` 处分帧、对**完整帧**做
lossy 解码。帧分隔符 `\n\n`(0x0A 0x0A)不可能出现在多字节 UTF-8 序列
中间(续字节 ≥ 0x80),字节级搜索安全。

### Bug 2「覆盖置换」= daemon 跨线程事件失序 + 客户端单段跟踪(实锤)

**发射端失序**(`aura-core/src/pipeline.rs`,`run()` spawn 的线程组):

| 线程 | 直发事件 | 延迟 |
|---|---|---|
| `aura-pipeline` | `StreamFragment`(inline 直发,run 消费循环) | 实时 |
| `aura-stage2` | `BatchSentence` / `SentenceCalibration` / `BatchParagraph` / `ParagraphCalibration`(Finalizer 循环) | LLM/batch,秒级 |

两个线程并发调 `on_turn` → `broadcast.send`,**无跨线程顺序协调**。
代码注释自认:"BatchParagraph 允许与部分 BatchSentence 交错(前端按 id
折叠,voice_state.rs 已鲁棒)" —— 该鲁棒性只对**同段内** id 折叠成立,
跨段顺序从未被保证。

**接收端脆弱**(`ime-core/src/family/magic/voice_state.rs`,`fold_event`):

- `current_paragraph` 是**单段跟踪**:新段的 `StreamFragment` 一到,
  `current_paragraph` 立刻切走,preview 跟着切走;
- `finals` 只由 `ParagraphCalibration` 写入(迟到中)。

失序窗口:用户停顿后继续说 → 段 N+1 的流式事件**先于**段 N 的
`ParagraphCalibration` 到达 → 旧段 N 既不在 finals(定稿在路上)也不在
preview(已切走)→ **从候选里凭空消失数秒**,直到迟到定稿补进 finals。
正是"上一个语音结果被新句子覆盖置换"。

隐藏次级 bug:现状 `finals.insert(0, …)` 按**到达序**插头部 —— 跨段
定稿乱序到达时(段 N+1 定稿先到、段 N 定稿后到)finals 顺序会**颠倒**。

### 次生缺陷: broadcast Lagged 静默丢帧(记录,本轮不修)

`apps/audio-aura/src/main.rs` `asr_stream`:订阅者消费过慢溢出
broadcast(1024)时,handler 发 `comment("lagged")` 继续走 —— 事件**静默
丢失**,客户端无感知。容量难打满,概率低;S2 落地后客户端具备按 id
自愈能力(重连 sync_history 语义),风险进一步降低。留观。

## 三、修复方案

### S1 — client.rs 字节级分帧(小,独立,先修 Bug 1)

`sse_data` 读循环改造:

```rust
let mut buf: Vec<u8> = Vec::new();
while let Some(Ok(chunk)) = bytes.next().await {
    buf.extend_from_slice(&chunk);
    while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
        let frame: Vec<u8> = buf.drain(..idx + 2).collect();
        // 完整帧才解码 —— 撕裂多字节字符不再可能。
        let s = String::from_utf8_lossy(&frame);
        for payload in data_payloads(&s) { yield payload.to_string(); }
    }
}
```

`client.rs` 现有 2 个单测护航;新增一个**多字节字符跨 chunk 撕裂**的
回归测试(把中文 JSON 从字节中间切开喂入,断言文本无损)。

### S2 — transcript 状态机归位 aura-agent + id 全序(修 Bug 2 + 架构归位)

用户指定的方向:**语音事件的细节处理归 AuraAgent 模块,上层不关注**。
现状 `ime-core/family/magic/voice_state.rs`(570 行)的五类事件折叠/
段落组装是 aura 协议细节,住在消费者里 —— 反了。

**目标结构**:

| 位置 | 内容 |
|---|---|
| `aura-agent/src/transcript.rs`(新增) | `Transcript`(纯状态机,零 tokio 零锁):五类事件折叠、段落状态、预览/定稿输出;`SharedTranscript`(Arc+Mutex 壳 + conn 三态 + mock 标志) |
| `ime-core/family/magic/voice_state.rs` | 删除;ime-core 改 `use audio_aura_agent::SharedTranscript`(类型别名 `SharedVoiceState` 过渡或全量改名) |

**核心语义升级 —— "id 即顺序"(跨段乱序彻底鲁棒,不依赖后端改动)**:

aura 端 `paragraph_id` 单调递增,它是比"到达顺序"更强的顺序信号。
`Transcript` 用 `BTreeMap<paragraph_id, ParaState>` 持段:

1. **finals 按段 id 排序**(替代现状的到达序头插):迟到定稿归位到正确
   顺序,次级 bug 一并修掉;
2. **首选组合预览 = "最后一个已定稿段落之后的所有段落" 的 best 文本按
   id 拼接**(每段 best = `ParagraphCalibration` > `BatchParagraph` >
   逐句拼接)。失序窗口里的表现:
   - 段 N 已关闭未定稿 + 段 N+1 开流 → 首选 = "N best + N+1 流式"
     —— **旧句不消失,新句无缝续上**;
   - 段 N 的迟到定稿到达 → N 进 finals,首选平滑收缩为 N+1;
3. 连接三态 / mock(`--asr-text` 冻结 conn + 种子数据)语义随迁
   `SharedTranscript`(测试与宿主调试都要种子注入)。

**API 对照**(消费者 `magic/voice.rs` 改动最小化):

| 现 `SharedVoiceState` | 新 `SharedTranscript` |
|---|---|
| `fold_event(&AsrEvent)` | 同名 |
| `voice_candidates() -> (Vec<String>, String)` | `finals()`(id 序,最新在前)+ `plain_preview()` |
| `preview() -> Option<AsrPreview>` | `plain_preview()` / `calc_preview()` |
| `conn()` / `set_conn()` | 同名 |
| `reset()` / `sync_history()` | 同名(sync_history 同样按 id 归位) |
| `snapshot()` / `set_mock` / `is_mock` / `seed_final` / `set_live_raw` | 同名随迁 |

**接线点迁移**:`io_thread.rs::VoiceSession`(fold/set_conn/reset/
sync_history/is_mock)、`magic/voice.rs`(候选构建)、`engine.rs`(mock
接线)—— 只改 use 路径与 API 名,逻辑不动。

### S3 — daemon 保序手术(暂缓,留观)

pipeline 出口加全局序号 / 跨段握手。S2 的 id 全序让客户端**不再依赖**
到达顺序,S3 大概率永远不必做。若后续发现 aura 端其他消费者(web SPA)
也受失序之苦再立项。

## 四、测试与验收

- S1:client.rs 分帧回归测试(中文跨 chunk)+ 现有 2 单测;全仓测试
  基线不变(ime-core 160+21+2,swift-ime 7+12+15),clippy 零警告。
- S2:voice_state.rs 现有 11 个折叠测试**随逻辑搬** aura-agent(改用
  Transcript API);新增乱序回归:① N+1 流式先于 N 定稿 → N 文本仍在
  首选且顺序正确;② 定稿乱序到达 → finals 按 id 序不颠倒。
- 手工验收路径:`#asr` 实时听写 → 停顿续说 → 候选不丢单;长句观察无 `�`。

## 五、分步提交

| 步 | 内容 | 提交 |
|---|---|---|
| 0 | 本文档 | `docs(aura): round11 立项 — ASR 显示 bug 调查与 transcript 归位` |
| S1 | client.rs 字节级分帧 + 撕裂回归测试 | `fix(aura-agent): SSE 字节级分帧 — 中文跨 chunk 不再撕裂` |
| S2 | transcript.rs 归位 + id 全序 + 消费者接线 | `refactor(ime): 语音事件折叠归位 aura-agent — id 全序,跨段乱序鲁棒` |
