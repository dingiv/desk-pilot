# dp-router 迁移施工文档

> **状态**:🟡 规划(2026-08-28)。本文档是**施工 checklist**,不是设计文档;设计依据见本文档
> "关键约束"段及现存 `docs/dp-models.md`(将被本文档取代)。

## 一、目标

将 desk-pilot 的本地模型服务层,从 **LocalAI (Go)** 切到自建的 **dp-router (Rust) + llama.cpp**。
同时统一 host 策略:**所有 transformer 风格的模型** 一律走 llama.cpp;**ONNX 生态**(语音栈)
继续在 Rust 进程内 host。

| 切前 | 切后 |
|---|---|
| `apps/dp-models` (Go + LocalAI 库, fiber HTTP) | `apps/dp-router` (Rust + axum) |
| 上游:LocalAI 统一管理(llama-cpp/transformers/vLLM 后端) | 上游:llama.cpp 的 `llama-server` 子进程 + 远程 OpenAI 兼容 HTTP |
| `crates/dp-models::MistralLlm` (mistralrs 本地 GGUF) | 删除 |
| `aura-core` LLM 默认走 `MistralLlm` (本地嵌入) | `aura-core` LLM 默认走 `HttpLlm → dp-router:8080` |
| `aura-core` 依赖 `mistralrs/cuda` 重依赖 | 卸下 |
| Go toolchain 依赖 | 卸下(全程 Rust + Python 构建脚本) |

## 二、新架构

```
┌─────────────────────────────────────────────────────────────────────┐
│  desk-pilot 客户端                                                   │
│                                                                     │
│   ┌─────────────────────────┐    ┌──────────────────────────┐       │
│   │  Rust 内部 app          │    │  TS / 其他外部客户端      │       │
│   │  (audio-aura,           │    │  (@vrover/providers 等)  │       │
│   │   aura-core, 未来 ...)  │    │                          │       │
│   │                         │    │                          │       │
│   │  ┌──────────────────┐  │    │  ┌──────────────────┐    │       │
│   │  │ crates/dp-models │  │    │  │ @vrover/providers│    │       │
│   │  │  (内部 SDK)       │  │    │  │ (OpenAI 兼容)    │    │       │
│   │  └──────────────────┘  │    │  └──────────────────┘    │       │
│   └──────────┬─────────────┘    └─────────────┬────────────┘       │
└──────────────┼──────────────────────────────────┼──────────────────┘
               │                                  │
               │ crates/dp-models ─X─→ dp-router  │  (无直接依赖,仅协议)
               │                                  │
               │           OpenAI 兼容 HTTP        │
               └──────────────┬───────────────────┘
                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│  apps/dp-router (Rust, axum)            :8080                       │
│                                                                     │
│   ┌────────────────────────┐    ┌─────────────────────────────┐     │
│   │  控制面 (axum 路由)     │    │  本地模型进程管理           │     │
│   │  /v1/chat/completions  │←→│ LlamaProcess (tokio)       │     │
│   │  /v1/models            │    │   spawn llama-server        │     │
│   │  /admin/models        │    │   health check + restart    │     │
│   └────────────┬───────────┘    └──────────────┬──────────────┘     │
│                │                               │                    │
│                │ 按 model 名字段路由            │ spawn              │
│                ▼                               ▼                    │
│   ┌────────────────────────┐    ┌─────────────────────────────┐     │
│   │  远程上游代理           │    │  llama-server 子进程         │     │
│   │  (reqwest)              │    │  :18001 / :18002 / ...      │     │
│   └────────────┬───────────┘    └─────────────────────────────┘     │
└────────────────┼─────────────────────────────────────────────────────┘
                 │
                 │ HTTP (OpenAI 兼容)
                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│  远程 OpenAI 兼容上游 (vLLM / 远端 llama.cpp / 云端 API)            │
└─────────────────────────────────────────────────────────────────────┘
```

## 三、关键约束

1. **`apps/dp-router` 与 `crates/dp-models` 无 cargo 直接依赖**
   - 两者通过 **OpenAI 兼容 HTTP 协议** 间接耦合
   - `dp-router` 的 Cargo.toml **不出现** `dp-models`;反之亦然(本来 `dp-models` 也不该依赖 server)
2. **协议是 OpenAI 兼容(子集)**
   - 必须覆盖 `crates/dp-models` 当前 `HttpLlm` / `HttpAsr` 已用的请求/响应形状
   - 不强求实现完整 OpenAI 规范;TS / 其他外部客户端后续按需扩展
3. **Transformer 风格模型一律 llama.cpp host**
   - LLM / VLM / 远程 transformer 一视同仁通过 llama.cpp GGUF + `llama-server`
   - 不再依赖 vLLM / TGI / SGLang / Transformers 后端
4. **ONNX 生态保留 Rust 进程内 host**
   - Silero VAD、Zipformer 流式 ASR、SenseVoice / Qwen3-ASR / Whisper batch ASR
   - 继续在 `crates/dp-models::onnx` (sherpa-onnx) 中,本轮不动
5. **llama-server 子进程隔离**
   - 每个本地模型一个 `llama-server` 子进程,独占端口(端口段 18001–18099, dp-router 自动分配)
   - `dp-router` 自己开 8080 作为统一对外端口
6. **路由策略按 `model` 字段**
   - 请求里 `model` 命中已加载本地模型 → 转发到对应子进程
   - 未命中 → 转发到配置的远程上游(若无远程上游,返 404)
7. **`aura-core` 默认走 dp-router**
   - `LlmSpec` 默认值改为 `Remote { endpoint: "http://127.0.0.1:8080" }`
   - `LlmSpec::Local` 路径本轮删除(原实现 `MistralLlm` 整体下线)

## 四、实施阶段

> 每阶段独立可验证(build + 烟测)。**任何阶段失败可停在当前阶段,不影响仓库其余部分。**

### 阶段 0 — 准备工作

- **目标**:建立基线,确认当前仓库状态干净、可构建
- **操作**:
  - `git status`(确认无未提交改动阻塞)
  - `cargo build --workspace --exclude swift-ime --exclude geek-familiar --exclude audio-aura-devtools`(按记忆里的暂缓 app 列表定向构建)
  - 跑现有 `cargo run -p dp-models --example remote -- --help`(确认 dp-models 现有 example 不破)
- **验证**:build 全绿,example 可执行

### 阶段 1 — 删除 Go 应用

- **目标**:移除非 Rust 资产
- **涉及文件 / 目录**:
  - 删除:`apps/dp-models/`(整个目录,含 `main.go` / `go.mod` / `go.sum` / `dp-models` 二进制 / `models/`)
  - 删除:`docs/dp-models.md`(被本文档取代)
  - 检查:根 `Cargo.toml` 的 workspace members、`README.md` 任何 `dp-models` 引用
- **操作**:
  - `rm -rf apps/dp-models`
  - `grep -rn "dp-models" --exclude-dir=node_modules --exclude-dir=target --exclude-dir=.git .` 排查残余引用
  - 清理根 `Cargo.toml` workspace 列表(若列入)
- **验证**:`cargo build --workspace` 通过;`grep dp-models` 在源码内无 Go 相关残留

### 阶段 2 — 构建 llama.cpp server

- **目标**:`thirdparty/llama.cpp` 编译出可用的 `llama-server` 二进制
- **操作**:
  - `cd thirdparty/llama.cpp`
  - 按其 `docs/build.md` 配 cmake + 编译 `llama-server` 目标
  - 产物路径示例:`thirdparty/llama.cpp/build/bin/llama-server`
  - 记录到本阶段文档末尾(实际产物路径)
- **验证**:`./build/bin/llama-server --help` 可执行;最小烟测:`./build/bin/llama-server -m ../../../assets/models/qwen2.5-3b-instruct-q4_k_m.gguf --port 18099 -c 2048` 启动后 `curl http://127.0.0.1:18099/v1/models` 返非空

### 阶段 3 — 新增 `apps/dp-router`(Rust)

- **目标**:Rust 二进制实现 spawn + 路由 + 转发
- **涉及文件**(全部新建):
  - `apps/dp-router/Cargo.toml`
  - `apps/dp-router/src/main.rs`(CLI 解析 + 启动入口)
  - `apps/dp-router/src/config.rs`(配置 schema)
  - `apps/dp-router/src/process.rs`(`LlamaProcess` 子进程管理)
  - `apps/dp-router/src/router.rs`(HTTP 路由 + 转发)
  - `apps/dp-router/src/upstream.rs`(远程 OpenAI 兼容代理)
  - `apps/dp-router/config/models.example.yaml`(配置示例)
- **Cargo.toml 关键依赖**:
  - `axum`、`tokio`、`reqwest`、`serde`、`serde_json`、`serde_yaml`、`anyhow`、`tracing`、`tracing-subscriber`
  - **不出现 `dp-models`**(关键约束)
- **行为细则**:
  - CLI:`dp-router [--config path] [--addr :8080] [--llama-server /path/to/llama-server]`
  - 启动:读 yaml → 对每个本地 model spawn `llama-server --port <auto> --model <gguf> ...` → 等 `/health` 通 → 注册到路由表
  - `/v1/chat/completions`:按 body 里 `model` 字段找本地;命中则 reqwest 转发到子进程端口;未命中则转发到 yaml 中声明的 `remote_upstream`(无则 404)
  - `/v1/models`:返回本地 + 远程已知模型清单
  - `/admin/models`:列出已加载子进程的运行快照(name/gguf/port/status/restarts)+ 目录中未加载模型
  - 子进程异常退出:tracing 记录 + 自动重启(指数退避,3 次后停止)
- **验证**:
  - `cargo build -p dp-router` 通过
  - 启动后 `curl http://127.0.0.1:8080/admin/models` 返 yaml 中声明的本地模型(状态 online)
  - `curl http://127.0.0.1:8080/v1/chat/completions -d '{"model":"qwen2.5-3b","messages":[...]}'` 收到非空回复

### 阶段 4 — `crates/dp-models` 简化

- **目标**:删除 `MistralLlm` 与相关 feature,保留并复用现有 OpenAI 兼容 HTTP 客户端
- **涉及文件**:
  - 删除:`crates/dp-models/src/mistral.rs`
  - 修改:`crates/dp-models/Cargo.toml` — 删除 `mistralrs` / `tokio`(可选)/ `shared`(shared 只用于 `load_default`,重写或删除) 依赖与 `mistral` / `cuda` feature
  - 修改:`crates/dp-models/src/lib.rs` — 删除 `pub mod mistral;` 与 `pub use mistral::MistralLlm;`
  - 修改:`crates/dp-models/src/config.rs` — `ProviderKind::Local` 变体若仅 mistral.rs 使用则一并移除
  - 保留:`crates/dp-models/src/http.rs`(OpenAI 兼容 HTTP 客户端)— **协议形态不变**,只是默认 endpoint 在调用方改
  - 保留:`crates/dp-models/src/onnx.rs`(语音栈) — 本轮不动
- **验证**:
  - `cargo build -p dp-models` 通过(默认 features)
  - `cargo build -p dp-models --features speech` 通过(ONNX 路径)
  - `cargo run -p dp-models --example remote` 仍能编译运行(endpoint 默认改 8080)

### 阶段 5 — `aura-core` 切换到 Remote

- **目标**:aura-core / audio-aura 不再依赖 mistral.rs / cuda
- **涉及文件**:
  - 修改:`crates/aura-core/Cargo.toml` — 删除 `mistral` / `cuda` feature 与相关转发
  - 修改:`crates/aura-core/src/lib.rs` — `Calibrator` 内部不再持 `MistralLlm`,改为 `HttpLlm`(由调用方注入)
  - 修改:`crates/aura-core/src/pipeline.rs` — `LlmSpec::Local` 分支删除,默认 `Remote { endpoint: "http://127.0.0.1:8080" }`
  - 修改:`apps/audio-aura/Cargo.toml` — `required-features = ["asr"]`(去掉 `cuda`)
- **行为细节**:
  - `aura-core` 启动时不再自动加载 GGUF;由外部保证 dp-router 已就绪
  - `aura.yaml` 中 LLM 配置默认 `llm.backend: remote` + `endpoint: http://127.0.0.1:8080`
- **验证**:
  - `cargo build -p audio-aura` 通过
  - `cargo build -p audio-aura-core` 通过
  - aura daemon 启动后用 `HttpLlm` 调 dp-router 的 `/v1/chat/completions` 端到端烟测(沿用 `crates/dp-models/examples/remote.rs` 模式)

### 阶段 6 — 文档更新

- **目标**:决策、运行说明齐备
- **涉及文件**:
  - 删除:`docs/dp-models.md`(被 `dp-router-migration.md` 取代)
  - 新建:`docs/dp-router.md`(dp-router 用户文档:配置项、CLI、典型部署、与 llama.cpp 版本对齐策略)
  - 修改:`docs/README.md` 或 docs index 索引,指向新文档
  - 修改:`CLAUDE.md`(若其中提到 `dp-models` Go 应用需同步)
- **验证**:文档能解释新架构、新启动方式、新依赖关系

### 阶段 7 — 验证 + 记忆更新

- **目标**:全栈联调 + 知识沉淀
- **操作**:
  - 全 workspace 构建:`cargo build --workspace --exclude swift-ime --exclude geek-familiar --exclude audio-aura-devtools`
  - 端到端烟测:
    1. 启动 `dp-router`(配置含 `qwen2.5-3b` 本地模型)
    2. `crates/dp-models/examples/remote.rs` 调 `HttpLlm::new("http://127.0.0.1:8080", "qwen2.5-3b")` 拿到非空回复
    3. 启动 audio-aura daemon,用真实 ASR → LLM 链路产出文本
  - 记忆:新增 `dp-router-architecture.md` 至 `~/.claude/projects/-workspaces-gui-agent/memory/`,同步 `audio-aura-project.md` 删去 mistral.rs 段
- **验证**:端到端通过,记忆索引更新

## 五、文件变更清单

### 删除
- `apps/dp-models/`(整目录)
- `crates/dp-models/src/mistral.rs`
- `docs/dp-models.md`

### 新建
- `apps/dp-router/Cargo.toml`
- `apps/dp-router/src/main.rs`
- `apps/dp-router/src/config.rs`
- `apps/dp-router/src/process.rs`
- `apps/dp-router/src/router.rs`
- `apps/dp-router/src/upstream.rs`
- `apps/dp-router/config/models.example.yaml`
- `docs/dp-router-migration.md`(本文档)
- `docs/dp-router.md`(用户文档,阶段 6)

### 修改
- `Cargo.toml`(workspace members 若涉及)
- `crates/dp-models/Cargo.toml`(删依赖与 feature)
- `crates/dp-models/src/lib.rs`(删 mistral 模块声明与 re-export)
- `crates/dp-models/src/config.rs`(`ProviderKind` 简化)
- `crates/aura-core/Cargo.toml`(删 mistral/cuda feature)
- `crates/aura-core/src/lib.rs`(`Calibrator` 持有 `HttpLlm`)
- `crates/aura-core/src/pipeline.rs`(`LlmSpec::Local` 删除)
- `apps/audio-aura/Cargo.toml`(`required-features`)
- `apps/audio-aura/aura.yaml`(LLM 默认 backend 改 remote)
- `CLAUDE.md`(若有 Go / LocalAI 引用)

### 暂不动
- `crates/dp-models/src/onnx.rs`(语音栈)
- `crates/dp-models/src/http.rs`(OpenAI 兼容客户端,形态不变)
- `crates/dp-models/examples/remote.rs`(仅默认 endpoint 调整)
- `thirdparty/llama.cpp/`(仅构建产物,源码不动)
- TS 端(`packages/providers` 等)— 后续按需切

## 六、风险与回滚

| 风险 | 缓解 | 回滚 |
|---|---|---|
| llama.cpp 编译失败 / 产物在容器内缺依赖 | 阶段 2 独立验证,失败则本轮暂停不进入后续 | 保留 `apps/dp-models` 不删,代码改动 revert |
| dp-router spawn 子进程异常(端口冲突 / 路径错) | 阶段 3 独立烟测,失败则只回滚 dp-router crate | 同上 |
| aura-core 切到 Remote 后 Stage2 延迟 / 准确率回归 | 阶段 5 端到端烟测覆盖 Stage1→batch→Stage2 全链路 | 保留 `MistralLlm` 模块 git history,临时切回 |
| ONNX 语音栈被意外改动 | 阶段 4 明确只动 `mistral.rs` 与 `Cargo.toml` features | git diff 校验 |

**回滚原则**:任何阶段发现非预期影响,**不进入下一阶段**,停在当前阶段定位;不跨阶段回滚。
若已跨阶段,优先 `git revert` 单个 commit / 单个 PR。

## 七、未决项(施工时再定)

- **A. llama-server 端口段**:默认 18001–18099;是否需要可配置
- **B. 子进程异常重启策略**:退避上限、重启后是否广播 `/admin` 状态变更
- **C. 远程上游认证**:yaml 字段 `api_key` 透传到上游(暂不做统一鉴权)
- **D. 流式响应**:`/v1/chat/completions` 是否本轮支持 `stream=true`(默认 SSE)— 待定
- **E. dp-router metrics**:`/metrics` Prometheus 端点 — 不在本轮范围
- **F. `crates/dp-models` 是否加 `endpoint` 默认值**:建议加 `DEFAULT_DP_ROUTER_ENDPOINT = "http://127.0.0.1:8080"` 简化调用方

## 八、施工顺序建议

```
阶段 0 → 阶段 1 → 阶段 2 → (阶段 3 + 阶段 4 并行,互不依赖) → 阶段 5 → 阶段 6 → 阶段 7
```

- 阶段 3 / 4 可并行:dp-router 是新 crate,crates/dp-models 是删减,二者无 cargo 依赖
- 阶段 5 必须在 3 / 4 完成后(因为它依赖新的 dp-models + dp-router 已存在)