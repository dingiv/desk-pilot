# 客户端状态同步设计：控制面 / 数据面 / 按需

> 2026-08。客户端（web UI / `audio-aura-agent::AuraClient` SDK / desktop-pet）与 daemon 之间
> 同步哪些状态、各自走什么传输。先于 SDK API 定型——把状态盘清楚，传输自然落地。
> 北极星：[[ai-secretary-north-star]]。现状见 [[aura-stateview-snapshot-api]] memory。

## 一、原则：按"频率 × 数据量"选传输，不要一刀切

两种极端都不对：

- **全走一个大快照**（snapshot-sync：每次变化 GET 整个状态）—— 对**高频小数据**（流式 partial
  每 ~0.5s 一次）就变成"每秒数次拉整个状态"，浪费带宽 + 多一次 ping→GET 往返延迟。
- **全走一个事件流**（push every event）—— 对**低频/不变的 setting**（config 启动后不动）
  白白维持一条流；对**大块二进制**（音频 WAV）push 流也不合适。

所以每个状态按两个轴归类，落到三种传输之一：

| 轴 | 数据面 push 流 | 控制面 snapshot-sync | 按需 GET |
|---|---|---|---|
| 更新频率 | 高（秒级以下） | 低（分钟级 / 启动一次 / 用户动作） | 低 / 按需 |
| 数据量 | 小（文字片段） | 小（settings） | 大（二进制）/ 查询 |
| 机制 | 每事件直推、不节流 | 节流 ping(≥250ms) → GET 整快照 | 客户端主动拉 |

## 二、状态清单（盘点 + 分类 + 归属）

### 数据面（`GET /api/asr_stream` → `AsrSegment` 流，每事件直推）

| # | 状态 | 来源 | 频率 | 数据量 | 段类型 |
|---|---|---|---|---|---|
| 1 | 流式 partial（raw，前向纠错） | Stage1 Zipformer | **高**（~0.5s，说话中） | 小（~50–200 字） | `interim` |
| 2 | Stage2 碎片纠偏（provisional calibrated） | Stage2 `calibrate_provisional` | 中（每 VAD 碎片，~1–5s） | 小 | `calibrated_interim` |
| 3 | 定稿（raw / streaming / calibrated / intent / reply） | Stage2 `calibrate`（settle） | 低（每句 ~5–30s） | 小–中 | `final` |
| 4 | 用户纠偏标记（per-utterance） | `POST /api/correct` | 极低（用户动作） | 极小 | `correction` |

> **注意**：Stage1 的批式 provisional（`Revise`，累积 PCM 重跑的 raw 文本）是**内部**的——它
> 进 composer → Stage2，产出 `calibrated_interim`。客户端不需要 raw 批式文本（已有流式 raw +
> Stage2 纠偏），所以**不单独暴露**在数据面。

### 控制面（`GET /api/state` → `AuraStateView` 快照，节流 ping → 重拉）

| # | 状态 | 来源 | 频率 | 数据量 | 字段 |
|---|---|---|---|---|---|
| 5 | connected（scout 开关） | toggle | 低（用户动作） | 极小 | `connected` |
| 6 | config（asr/llm/vad 参数） | 启动加载 `aura.yaml` | **~不变**（启动一次） | 小 | `config` |
| 7 | hotwords 列表 | Stage3 加词 | 低–中（每含专名的句） | 小–中（**无 cap，会增长**） | `hotwords` |
| 8 | corrections 列表（raw→corrected，Stage2 反馈） | `POST /api/correct` | 极低 | 小（cap 20） | `corrections` |

### 按需（客户端主动 GET，不进任何流/快照）

| # | 状态 | 频率 | 数据量 | 端点 |
|---|---|---|---|---|
| 9 | 音频 WAV（per-utterance 原声） | 低（每句） | **大**（16kHz mono，5s≈160KB，长句→MB） | `GET /api/audio/{seq}` |
| 10 | 录音列表（所有 seq） | 低（每句增长） | 小（id 列表） | `GET /api/recordings` |

## 三、Stage2 的边界（关键澄清）

Stage2 产生两类东西，**归不同的面**，不能混：

```
Stage2
 ├─ 输出：纠偏后的文字 ──→ 数据面（识别结果，高频小数据，低延迟直推）
 │    · provisional（每碎片）→ AsrSegment::CalibratedInterim
 │    · final（定稿）       → AsrSegment::Final.calibrated
 │
 └─ 输入配置：corrections 反馈 ──→ 控制面（一个 setting，低频小数据，Stage2 每轮读）
      · 用户教 Stage2 怎么纠的累积 → AuraStateView.corrections
```

所以 Stage2 的**文字输出绝不进 settings 快照**（解耦后已是这样：utterances 不在 AuraStateView
里）。混进去的旧设计已经被纠正。

**"用户纠偏"这一个动作会碰两面**，因为它的两个消费者不同：
- 控制面 `corrections` 列表 → Stage2 读（配置反馈）。
- 数据面 `correction` 段 → UI 把那句标 `corrected_by_user`（per-utterance 显示）。

这不是重复，是同一动作的两个视图。

## 四、待定 / 可改进点（设计决策，未实现）

1. **hotwords 无 cap 会增长** —— Stage3 每个含专名的句子都加词，长会话下 `hotwords` 无限增长，
   每次快照重拉全量。→ cap（LRU）或控制面只推"近期新增 + 总数"。**待定**。
2. **corrections 列表是否真该在快照** —— 它是 Stage2 内部反馈。客户端真需要看吗？若只为
   "纠偏历史"面板可留；否则属 internal，可挪到按需或不下发。**待定**。
3. **数据面单流 vs 可过滤** —— 现在 4 种段共用一条 `/api/asr_stream`（`type` 区分）。客户端想要
   "只要 final、不要 interim"没法过滤。→ 加 query param 过滤 type，或拆流。目前单流够用。**待定**。
4. **late-joiner catch-up** —— 数据面是 push 流，**中途连接的客户端只看到连接后的事件**，错过历史
   utterances；而控制面快照已无 utterances（解耦后），所以新客户端无法回看历史。→ 选项：(a) 数据面
   连接时先回放最近 N 条 `final`；(b) 快照保留"近期 finals"（只 final，不含 live）。dev UI 重连就空
   可接受；**正式客户端必须解决**。**待定**。
5. **节流参数粒度** —— `state_changed_frequency` 是全局的。若 hotwords 高频变而 config 不变，仍按
   全局节流。一般够用；真需要可按"scope"分别节流。**低优**。

## 五、推导出的 SDK 形态（现状 + 待加）

现状（`audio-aura-agent::AuraClient`）已覆盖核心：

| SDK 方法 | 面 | 产 | 状态 |
|---|---|---|---|
| `subscribe_segments()` | 数据面 | `Stream<AsrSegment>` | ✅ |
| `subscribe(freq_ms)` | 控制面 | `Stream<AuraStateView>` | ✅ |
| `state()` / `set_connected()` / `correct()` / `audio(seq)` / `recordings()` | 按需/动作 | — | ✅ |

待 §四 决议后可能再加：
- `subscribe_segments(filter)` —— 按 type 过滤（点 3）。
- late-joiner 回放（点 4）—— 可能在 SDK 内部：连接时先 `state()` 拿近期 finals，再接 segments。

SDK API 的**形态**（订阅两路 + 按需）已经稳了；剩下的都是"推什么 / 怎么过滤 / 怎么 catch-up"的内容
问题，不是 API 形状问题。所以**先不急着改 SDK**，等 §四 几个点拍板。

## 六、为什么这样分（一句话总结）

> **把"正在生成的识别文字"和"系统的静态配置"分到两条物理通道**——前者高频要直推、后者低频要节流；
> **大块二进制永远按需拉**。三者各得其所，客户端按需订阅，不互相拖累。
