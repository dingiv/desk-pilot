# dp-models 整改方案：模型生命周期管理库

> 状态：🟡 **设计 / 未实现**（2026-07-23）。当前 dp-models 已是"trait + remote"抽象层（见
> `crates/dp-models/`），本文是把它**升级为模型生命周期管理库**的整改方案。实现时以代码为准。

## 背景（为什么）

desk-pilot 多子系统（aura 语音 / visual-rover 视觉 / 未来更多）都要本地或远程模型推理。现状
是**模型启动逻辑分散**：aura-asr 自己 `OnnxAsr::new`（sherpa）、aura-dcl 自己
`Calibrator::load_default`（mistral.rs）、daemon 在 `main.rs` 手写 `if asr_kind / if llm_kind`
工厂拼装。每加一个模型 / 换一个后端，daemon 和各 crate 都要改。

dp-models 已经把"调用"统一成了 Provider trait（AsrProvider/LlmProvider/VlmProvider + Http*
remote），但**启动/加载仍散在各 crate**。本方案把"启动"也收进来——dp-models 成为**唯一的模型
入口**，上层只声明"要什么模型"，dp-models 负责启动/连接并返回 Provider。

## 定位

**dp-models = 模型生命周期管理库**（启动 + 抽象）：
- 向上层提供统一的 **Provider 抽象**，4 个子类：`Asr` / `Llm` / `Tts` / `Vlm`。
- 支持 **3 种后端**启动模型：
  - `Onnx` — 通过 sherpa-onnx 启动本地 onnx 模型（ASR / 未来 TTS）。
  - `MistralRs` — 通过 mistral.rs 启动本地 GGUF 模型（LLM / 未来 VLM）。
  - `Remote` — 远程连接（HTTP，OpenAI 兼容，API key 认证）。
- 上层提供**构造参数**（模型类型 + 名称 + 后端类型 + 后端参数），dp-models 选后端启动。

上层（daemon / visual-rover app）不再 `OnnxAsr::new` / `Calibrator::load_default` / 手写
if/else，只：
```rust
let asr = dp_models::build_asr(ModelSpec{ task:Asr, name:"qwen3-asr-1.7b", backend:Backend::Onnx, params:.. })?;
let llm = dp_models::build_llm(ModelSpec{ task:Llm, name:"Qwen3-1.7B-Q8_0.gguf", backend:Backend::MistralRs, params:.. })?;
```

## 目标架构

```
dp-models/  (模型生命周期管理库)
├── lib.rs                  Provider trait (4) + ModelSpec + Backend + build() 工厂
├── spec.rs                 ModelSpec / Task / Backend / BackendParams
├── factory.rs              build_asr / build_llm / build_tts / build_vlm (按 spec 选后端)
├── backend/
│   ├── mod.rs              Backend trait (start() → Provider)
│   ├── onnx.rs             onnx 后端 (feature onnx, sherpa-onnx): OfflineRecognizer/TTS
│   ├── mistral.rs          mistral.rs 后端 (feature mistralrs): GGUF LLM
│   └── remote.rs           remote 后端 (默认): Http* + Bearer api_key
├── http.rs                 Http* (remote 实现, 已有)
└── config.rs               ProviderKind (已有, 保留向下兼容)
```

**依赖**：dp-models 不再是纯叶子——`onnx` / `mistralrs` feature 分别拉 sherpa-onnx / mistral.rs
（重依赖，按需开）。`remote` 后端是默认（轻，只 reqwest）。

## 核心抽象

### 4 个 Provider trait（已有 3 个，加 Tts）

```rust
pub trait AsrProvider: Send + Sync { fn recognize(&self, pcm: &[i16], sr: u32) -> Result<String>; }
pub trait LlmProvider: Send + Sync { fn complete(&self, system: &str, user: &str) -> Result<String>; }
pub trait TtsProvider: Send + Sync { fn synthesize(&self, text: &str) -> Result<Vec<i16 /* pcm */>>; }  // 新增
pub trait VlmProvider: Send + Sync { fn complete(&self, system:&str, user:&str, image_png:&[u8]) -> Result<String>; }
```

### ModelSpec（上层构造参数）

```rust
pub enum Task { Asr, Llm, Tts, Vlm }

pub enum Backend {
    Onnx,                                      // 本地 sherpa-onnx
    MistralRs,                                 // 本地 mistral.rs GGUF
    Remote { endpoint: String, api_key: Option<String> },  // 远程 OpenAI 兼容
}

pub enum BackendParams {
    Onnx(OnnxParams),        // { model_paths, tokens, language?, backend_kind, threads, provider }
    Mistral(MistralParams),  // { model_path (GGUF) }
    Remote(RemoteParams),    // { model_id (传给 API), api_key }
}

pub struct ModelSpec {
    pub task: Task,
    pub name: String,            // 模型名 (qwen3-asr-1.7b / Qwen3-1.7B-Q8_0.gguf / 远程 model id)
    pub backend: Backend,
    pub params: BackendParams,
}
```

### build 工厂（feature-gated）

```rust
// 按 task 分 4 个工厂（返回值类型不同）；内部按 backend 选实现。
pub fn build_asr(spec: ModelSpec)  -> Result<Arc<dyn AsrProvider>>;
pub fn build_llm(spec: ModelSpec)  -> Result<Arc<dyn LlmProvider>>;
pub fn build_tts(spec: ModelSpec)  -> Result<Arc<dyn TtsProvider>>;
pub fn build_vlm(spec: ModelSpec)  -> Result<Arc<dyn VlmProvider>>;
```

## 3 后端详解

| 后端 | feature | 启动方式 | 覆盖 task | 依赖 |
|---|---|---|---|---|
| `Onnx` | `onnx` | sherpa-onnx OfflineRecognizer / OfflineTTS | ASR ✓, TTS（未来 Kokoro/Piper） | sherpa-onnx（重）|
| `MistralRs` | `mistralrs` | mistral.rs GgufModelBuilder | LLM ✓, VLM（未来 candle） | mistral.rs（重）|
| `Remote` | 默认 | reqwest::blocking Http* + Bearer | 全 4 task | reqwest（轻）|

## 现状 vs 目标

| 维度 | 现状 | 目标 |
|---|---|---|
| dp-models 职责 | trait + remote（纯抽象） | 启动 + 抽象（管理库）|
| dp-models 依赖 | 纯叶子（reqwest）| feature-gated 拉 sherpa/mistral.rs |
| OnnxAsr 加载 | aura-asr | → dp-models onnx 后端 |
| Calibrator 加载 | aura-dcl | → dp-models mistralrs 后端 |
| daemon 工厂 | main.rs 散 if asr_kind/llm_kind | 一行 `dp_models::build(spec)` |
| aura-asr/aura-dcl | 厚（加载+推理） | 变薄（Stage1 管线 / Stage2 提示词）|
| TTS | NoopTts 孤岛 | TtsProvider trait + onnx/remote 后端 |
| 模型路径 | shared FileLoader MODELS namespace（aura-asr/aura-dcl 各声明） | 经 ModelSpec.params 传入（路径解析仍用 shared）|

## 关键设计决策（推荐，待确认）

1. **dp-models 直接依赖 sherpa-onnx + mistral.rs（feature-gated）**。
   - 推荐 ✅：启动逻辑真集中，daemon 只依赖 dp-models。重依赖按 feature 开（不开 = 纯 remote，轻）。
   - 代价：OnnxAsr / Calibrator 加载逻辑从 aura-asr/aura-dcl 迁移过来。

2. **VAD / 流式 Zipformer 留 aura-asr（不进 dp-models）**。
   - 它们是 Stage1 管线特有（非"一个模型"），不属于 4 task Provider。
   - dp-models onnx 后端**只管 batch ASR**（OfflineRecognizer）。aura-asr 的 Stage1Executor 仍组装
     VAD + streaming + batch（batch 从 dp-models build_asr 拿）。

3. **BackendParams 用 enum（类型安全 + 可扩展）**。
   - 每后端一个 struct，ModelSpec.params: BackendParams。加新后端 = 加一个 enum variant。

4. **模型路径仍走 shared FileLoader**（dev `assets/models` / prod `~/.desk-pilot/models`）。
   - ModelSpec.params 里的路径由上层（daemon）用 `shared::loader!()` 解析后传入，dp-models 不
     自己解析 namespace（保持解耦）。

## 迁移影响（实现时要动的）

- **aura-asr**：`OnnxAsr`（+ OfflineRecognizer 构造逻辑）移到 dp-models `backend/onnx.rs`。
  aura-asr 保留 VAD / streaming / Stage1Executor / `Asr` re-export（`Asr = AsrProvider`）。
- **aura-dcl**：`Calibrator`（GGUF 加载）移到 dp-models `backend/mistral.rs`。aura-dcl 保留
  `Stage2CalibratorImpl`（提示词 / ContextWindow / 热词）+ `impl LlmProvider`（或直接用 dp-models）。
- **aura-tts**：NoopTts `impl TtsProvider`；未来 sherpa Kokoro/Piper 在 dp-models onnx 后端。
- **apps/audio-aura**：删 main.rs 的 ASR/LLM if/else 工厂，改 `dp_models::build_*(spec)`。aura.json
  从 `asr_kind/llm_kind` 升级为 `ModelSpec` 描述（task+name+backend+params）。

## 分步实施（建议）

1. **dp-models 加 onnx 后端**（feature onnx）：移 OnnxAsr 加载 → `backend/onnx.rs` + `build_asr`。
   aura-asr 的 OnnxAsr 改为 re-export / 委托 dp-models。daemon asr 走 `build_asr`。
2. **dp-models 加 mistralrs 后端**（feature mistralrs）：移 Calibrator 加载 → `backend/mistral.rs`
   + `build_llm`。daemon llm 走 `build_llm`。
3. **加 TtsProvider trait + build_tts**：aura-tts NoopTts impl；未来 sherpa TTS 填 onnx 后端。
4. **daemon 工厂重写**：aura.json 升级为 ModelSpec 描述，main.rs 删 if/else，全走 `build_*`。
5. **（可选）model registry**：dp-models 加"模型目录"（id → spec），daemon 只传 id。等
   多模型切换需求再做。

每步独立可验证（编译 + 回归）。

## 开放问题

- **candle 后端**？candle Qwen3-ASR PR#3509 merge 后，可能加第 4 个后端 `Candle`（纯 Rust，绕过
  onnxruntime）。先观察，merge 后再评估。
- **API key 管理**：remote 后端的 api_key 存哪（aura.json 明文 / env / keyring）？建议 env 优先，
  aura.json 可选。
- **模型热切换**：daemon 运行时换模型（不重启）？当前 build 一次性，热切换要 Provider 可重建 +
  Pipeline 重启。后续需求。
