# dp-router — desk-pilot 本地 LLM 路由

> 状态:✅ **可用**(2026-08-28)。架构与决策依据见 [`docs/dp-router-migration.md`](dp-router-migration.md);
> 本文是**用户文档**——怎么跑、怎么配、怎么调。

## 一、职责

dp-router 是一个独立的 Rust 服务(Rust + axum),做三件事:

1. **本地模型运行管理**——按 yaml 配置 spawn 多个 `llama-server` 子进程(每个本地 GGUF 模型一个),
   独占端口段(`18001–18099`);管理生命周期、健康检查、异常重启。
2. **本地 HTTP 服务暴露**——对外开一个 OpenAI 兼容 HTTP 端口(默认 `:8080`),把请求按
   `model` 字段路由到对应子进程。
3. **远程模型代理路由**——未命中本地时 fallback 到配置的远程 OpenAI 兼容上游(可选)。

**Transformer 风格模型一律走 llama.cpp**(`llama-server`),**ONNX 生态**(语音栈)
继续在 `crates/dp-models` 的 Rust 进程内 host——**互不替代**。

## 二、架构

```
┌────────────────────────────────────────────────────────────┐
│  客户端(其它 app / 外部)                                       │
│   - audio-aura / aura-core (Rust, 通过 crates/dp-models)   │
│   - TS / OpenAI 兼容客户端                                    │
│                          │                                 │
│                          │  OpenAI 兼容 HTTP               │
│                          ▼                                 │
│  ┌──────────────────────────────────────────────────────┐ │
│  │  apps/dp-router :8080 (Rust, axum)                   │ │
│  │   POST /v1/chat/completions                           │ │
│  │   GET  /v1/models                                    │ │
│  │   GET  /admin/models                                 │ │
│  │   GET  /health                                       │ │
│  └────┬───────────────────────────────────────┬─────────┘ │
│       │ 按 model 字段路由                        │           │
│       ▼                                       ▼           │
│  ┌─────────────────────┐            ┌─────────────────┐  │
│  │ llama-server 子进程  │            │ 远程 OpenAI 兼容 │  │
│  │ :18001 / :18002 ... │            │ 上游 fallback   │  │
│  └─────────────────────┘            └─────────────────┘  │
└────────────────────────────────────────────────────────────┘
```

## 三、与 crates/dp-models 的关系(关键约束)

**`apps/dp-router` 与 `crates/dp-models` 不存在 cargo 直接依赖**(0 行 import),
二者只通过 **OpenAI 兼容 HTTP 协议** 间接耦合:

- `crates/dp-models` 的 `HttpLlm` / `HttpAsr` / `HttpVlm`(OpenAI 兼容 HTTP 客户端)→ 客户端视角
- `apps/dp-router`(实现 OpenAI 兼容子集)→ 服务端视角

任何一边的实现细节(数据结构、内部模块、命名)都不影响另一边;只有请求/响应 JSON 形状
是契约。修改任一侧前,先验证这个契约没破。

## 四、启动

### 4.1 一次性:构建 llama-server

```bash
cd thirdparty/llama.cpp
cmake -B build -DCMAKE_BUILD_TYPE=Release -DLLAMA_BUILD_SERVER=ON
cmake --build build --config Release --target llama-server -j 4
```

产物:`thirdparty/llama.cpp/build/bin/llama-server`(linux x86_64)。

### 4.2 准备配置文件

dev:`apps/dp-router/dp-router.yaml`(由 `shared::loader!()` 解析 `CONF::dp-router.yaml`)
prod:`~/.desk-pilot/dp-router.yaml`

模板见 `apps/dp-router/config/models.example.yaml`。最少配置:

```yaml
server:
  addr: "127.0.0.1:8080"

llama_server:
  path: "/abs/path/to/llama-server"

# 默认模型搜索根目录(供 POST /admin/models/load 动态加载按名寻路径)。
# 留空 = 不支持动态加载。
models_root: "/abs/path/to/models_root"

models:
  - name: "qwen2.5-3b-instruct-q4_k_m"
    gguf: "/abs/path/to/qwen2.5-3b-instruct-q4_k_m.gguf"
    context_size: 4096
    threads: 8
    gpu_layers: 0        # CPU;有 GPU 设 ≥ 1

remote_upstream:          # 可选
  base_url: ""            # 留空 = 关闭
  api_key: ""
  default_model: ""
```

`gguf` 与 `models_root` 字段均支持两种写法:
- `MODELS::xxx.gguf` / `MODELS::` —— `crates/dp-models` 的 `MODELS` 命名空间(dev: `assets/models/`,
  prod: `~/.desk-pilot/models/`)
- 绝对路径

`models_root` 留空 → **不支持动态加载**,只能从 `models:` 预加载列表启。

### 4.3 启动服务

```bash
cargo run -p dp-router
```

或 CLI 覆盖:
```bash
cargo run -p dp-router -- --addr :9090 --llama-server /opt/llama.cpp/build/bin/llama-server
```

### 4.4 验证

```bash
curl http://127.0.0.1:8080/health                # → "ok"
curl http://127.0.0.1:8080/admin/models          # 看子进程 status
curl -X POST http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen2.5-3b-instruct-q4_k_m","messages":[{"role":"user","content":"hi"}],"max_tokens":32}'
```

## 五、配置字段

### `server`
| 字段 | 含义 |
|---|---|
| `addr` | dp-router 对外监听地址(默认 `127.0.0.1:8080`) |

### `llama_server`
| 字段 | 含义 | 默认 |
|---|---|---|
| `path` | `llama-server` 二进制绝对路径 | — |
| `port_range` | 子进程端口分配区间(避开常见应用) | `[18001, 18099]` |
| `health_check_interval_s` | 子进程 `/health` 探活间隔 | `5` |
| `restart_max_retries` | 单模型最大重启次数,超限标记 offline | `3` |
| `restart_backoff_base_s` | 重启退避基数(实际等待 = base × 2^attempt) | `1` |

### `models[].*`(每个本地模型一个)
| 字段 | 含义 | 默认 |
|---|---|---|
| `name` | OpenAI 兼容请求里 `model` 字段的匹配键(**唯一**) | — |
| `gguf` | GGUF 文件路径(MODELS 命名空间或绝对路径) | — |
| `context_size` | 上下文长度(`-c`) | `4096` |
| `threads` | 推理线程数(`--threads`) | `8` |
| `gpu_layers` | GPU 卸载层数(`-ngl`);CPU = 0 | `0` |
| `batch_size` | 批大小(`-b`) | `512` |
| `extra_args` | 额外参数(高级,直接拼命令行) | `[]` |

### `models_root`(可选)
| 字段 | 含义 | 默认 |
|---|---|---|
| `models_root` | 默认模型搜索根目录;`POST /admin/models/load` 按名寻 GGUF 时使用。留空 = 不支持动态加载 | `""` |

### `remote_upstream`(可选)
| 字段 | 含义 | 默认 |
|---|---|---|
| `base_url` | 上游 base URL(OpenAI 兼容);**留空 = 关闭** | `""` |
| `api_key` | bearer token(原样透传) | `""` |
| `default_model` | 缺省 `model` 字段时的兜底 | `""` |

## 六、HTTP 端点

### POST `/v1/chat/completions`
OpenAI 兼容请求体。`model` 字段路由逻辑:
1. 命中已加载的本地子进程 → 转发到子进程 `:port`
2. 未命中 + `remote_upstream.base_url` 非空 → 转发上游
3. 都没匹配 → 404

**非流式**(本轮)。`stream=true` 不支持,后续按需扩展。

### GET `/v1/models`
返回**模型目录 ∪ 已加载子进程**的并集(按 name 去重、排序),与运行状态无关。**不**主动拉上游(避免启动阻塞)。

### GET `/admin/models`
返回本地子进程运行快照,便于诊断:
```json
{
  "router": "dp-router",
  "upstream_enabled": false,
  "models": [
    {
      "name": "qwen2.5-3b-instruct-q4_k_m",
      "gguf": "/abs/path/.../qwen2.5-3b-instruct-q4_k_m.gguf",
      "port": 18001,
      "status": "online",   // "online" | "offline" | "starting"
      "restarts": 0
    }
  ]
}
```

### POST `/admin/models/load`
**控制面**:SDK 主动拉起一个模型(无需重启 dp-router)。

请求体:
```json
{
  "name": "qwen3-asr-1.7b",          // 必填
  "gguf": "/abs/path/.../Qwen3-ASR-1.7B-Q4_K_M.gguf",  // 可选,显式路径优先
  "context_size": 4096,              // 可选,以下覆盖 llama-server 默认
  "threads": 8,
  "gpu_layers": 0,
  "batch_size": 512
}
```

服务端处理:
1. **已在表中** → 200,直接返当前快照(幂等)
2. `gguf` 字段在 body 里 → 解析后 spawn
3. 未提供 `gguf` → 在 `models_root` 下递归搜索(深度≤4、不跟符号链接、排除 `mmproj`),匹配优先级:文件 stem 与 `name` 完全一致 → 文件名包含 `name`(大小写敏感子串)
4. 都找不到 → 404 + JSON 错误
5. **资源耗尽** → 503(端口段用完)

成功 → **202 Accepted**(立即返)+ 状态快照;spawn 异步进行,SDK 随后轮询 `GET /admin/models`
直到 `status: "online"` 再发 `POST /v1/chat/completions`。

**典型工作流**:
```bash
# 1. 首次请求 → 命中(已预加载)
curl -s -X POST http://127.0.0.1:8080/v1/chat/completions -d '{
  "model":"qwen2.5-3b-instruct-q4_k_m","messages":[{"role":"user","content":"hi"}]
}'

# 2. 触发未预加载的模型(SDK 内部流程)
curl -s -X POST http://127.0.0.1:8080/admin/models/load -d '{
  "name":"qwen3-asr-1.7b"
}'    # → 202;服务端在 models_root 下找 GGUF + spawn

# 3. 轮询 /admin/models → status: "online" 后再发 chat
curl -s http://127.0.0.1:8080/admin/models | jq '.models[] | select(.name=="qwen3-asr-1.7b") | .status'
```

### GET `/health`
dp-router 自身健康检查。返回 `"ok"`。

## 六点五、ASR 后端(`/v1/audio/transcriptions`)

dp-router 现在还做 **OpenAI 兼容 ASR 路由**(llama.cpp multimodal:`llama-server --mmproj ...`)。`aura-daemon` 默认配置下,Stage1 流式 ASR 在 aura 进程内(sherpa-onnx,延迟敏感),**batch ASR 走 dp-router 上的 qwen3-asr 子进程**;Stage2 LLM 也走 dp-router 上的 qwen2.5-3b 子进程 —— **一个端口统一 LLM + ASR**。

`type: asr` 模型配置:
```yaml
models:
  - name: "qwen3-asr"
    type: asr                                          # 启用 ASR 后端(默认 llm)
    gguf: "MODELS::Qwen3-ASR-1.7B-Q4_K_M-GGUF/Qwen3-ASR-1.7B-Q4_K_M.gguf"
    mmproj: "MODELS::Qwen3-ASR-1.7B-Q4_K_M-GGUF/mmproj-Qwen3-ASR-1.7B-Q4_K_M.gguf"  # 必填
    context_size: 4096
    threads: 8
    gpu_layers: 0
```

`mmproj` 多模态投影器必填 —— dp-router 在 spawn 前校验文件存在性;`type=llm` 不需要。

**路由**:请求里 `model=qwen3-asr` → 命中本地 asr 子进程 → multipart 原样透传(llama.cpp 原生 `POST /v1/audio/transcriptions`,server.cpp:4982)→ 返 OpenAI 兼容 JSON。

**响应归一化**:llama.cpp 返 `{"type":"transcript.text.done","text":"...","usage":{...}}`(新 OpenAI responses 流式风格);dp-router **归一化** 为 OpenAI 标准 `{"text":"..."}`,`HttpAsr` 直接 parse。

### aura.yaml 配
```yaml
asr:
  backend: remote                              # 走 dp-router(替代旧 mloader)
  remote:
    endpoint: "http://127.0.0.1:8080"          # dp-router,统一 LLM + ASR
    model: "qwen3-asr"                        # dp-router.yaml models[].name
```

`HttpAsr::new(endpoint, model)` 把 `model` 拼进 multipart form —— OpenAI 规范要求。

## 七、典型工作流

### 与 audio-aura 联调

```bash
# 终端 1:起 dp-router(加载本地 LLM + qwen3-asr,统一 :8080)
cargo run -p dp-router

# 终端 2:起 omni-scout(audio 源)
cargo run -p omni-scout -- --port 7878 --mock-audio

# 终端 3:起 aura-daemon(Stage1 batch 走 dp-router qwen3-asr,Stage2 LLM 走 dp-router qwen2.5-3b)
./scripts/dev-up.sh start scout router aura
```

或者一键:`./scripts/dev-up.sh start all`。

`aura-daemon` 日志:
- `ASR: remote HTTP http://127.0.0.1:8080 (model qwen3-asr)` — batch 走 dp-router
- `Stage2 LLM: remote HTTP http://127.0.0.1:8080 (model qwen2.5-3b-instruct-q4_k_m)` — LLM 也走 dp-router
- 段定稿日志里 `batch="language Chinese<asr_text>..."` 来自 qwen3-asr;`streaming=...` 来自本地 zipformer
- 纠偏日志里 `calibrated=...` 来自 qwen2.5-3b LLM 整流

### 远程 fallback 验证

```yaml
remote_upstream:
  base_url: "http://192.168.1.6:8080"   # 局域网 vLLM
  api_key: "..."
```

请求本地不存在的模型名(如 `"qwen-remote-72b"`)→ 转发上游(LLM 和 ASR 都走 upstream)。

## 八、运维

- **健康检查**:`/health` 持续在线 + `/admin/models` 巡检子进程状态(LLM + ASR 共表)
- **重启**:异常退出会自动重启(指数退避,最多 `restart_max_retries` 次)
- **添加模型**:改 yaml → 重启 dp-router;**无热加载**(简化设计)
- **端口冲突**:改 `llama_server.port_range` 区间

## 九、限制(本轮)

- 不支持流式响应(`stream=true`)—— 后续按需加
- 管理面只有查询(`/admin/models`)和加载(`/admin/models/load`),无动态 unload HTTP 端点——重启 dp-router 即可
- 不实现完整 OpenAI 规范,只覆盖 SDK 用到的子集
- 无 metrics 端点(`/metrics` Prometheus)—— 后续按需