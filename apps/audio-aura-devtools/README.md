# 语音秘书 · Audio Aura

一个**语音输入 AI 秘书**的可运行雏形：你说话 → 实时识别 → **口语整流** → 秘书**判断你是闲聊还是下达任务** → 闲聊就回应、任务就**派给后端 worker 干活**、干完**汇报**。今天落地的第一个 worker 是「写作助手」——把你口述的零散素材重排、润色成一篇结构化文章。

> 设计蓝本见 `docs/chat.txt`。它描述的三阶段语音管线只是这个私人 AI 秘书的**最前站**（实时语音 + 日志/记忆结构）；秘书本身是一个只管"语音对话 + 意图理解"的前端代理，真正的活儿派发给 worker。本项目把这套结构做成了浏览器 Web 应用。

---

## 它是怎么工作的（三阶段 + 秘书）

```
浏览器（你的麦克风/音箱）                     后端 Node 进程（Koa）
Web Speech ASR ─每句话→ /api/turn → Stage1 落库（voice_chunks + 压缩音频，可重放溯源）
MediaRecorder(Opus)─音频→          │
                                   ▼  Stage2 语义整流（Calibrator, flash 模型，thinking 关）
                                      口语净化 / 抹语气词 / 相邻块合并 → calibrated_nodes
                                   ▼  ★ 秘书 Leader（意图路由）→ {intent: chat|task, reply, task?}
                        ┌──────────┴───────────┐
                   intent=chat             intent=task
                   秘书直接回应             建 task → 派发 worker
                   (可 TTS 朗读)                │
                                              ▼  Writer Worker（pro 模型，流式）
                                                 载入本 topic 的整流素材（长上下文）→ 结构化文章
                                                 → 回写 topic → 秘书汇报"写好了"
```

- **热回路（实时对话）** = Stage1 + Stage2：低延迟，保留近期完整时序。
- **冷回路（写作/长文）** = Stage3 Writer：长上下文重排润色，出好文章。
- 所有过程事件通过 **SSE**(`/api/stream`) 实时推到前端；对话区显示 `💬 闲聊 / 🛠️ 任务` 徽章和任务卡。

## 架构选择：级联 vs 端到端语音（S2S）

语音助手有两种底层架构，本项目**刻意选择级联**。

- **级联（Cascaded / Pipeline）**：三个独立模型串联，中间用**文字**当接口。
  `语音 →[ASR 听]→ 文字 →[LLM 想]→ 文字 →[TTS 说]→ 语音`
- **端到端语音（Speech-to-Speech, S2S）**：**一个模型**音频进、音频出，中间**没有文字环节**，直接在语音表示上思考（GPT-4o realtime、Moshi、Qwen-Omni、Gemini Live 等）。

| 维度 | 级联 | 端到端语音 S2S |
|---|---|---|
| 中间表示 | **文字**（可读、可拦截、可存） | 音频/语音向量（不透明黑盒） |
| 结构化输出 / 工具调用 | 天然（LLM 吐文本 → 校验 JSON、派 worker、写库） | 难，需旁路转写，成熟度低 |
| 可观测 / 可调试 | 每级都有产物 → 能定位"听错/想错/说错" | 音频进音频出，难查 |
| 延迟 | 优化后 ~500ms（靠流式接力） | 理论更低 ~200–500ms |
| 情感 / 韵律 / 全双工打断 | 转文字即丢失情绪 | 保留，自然打断 |
| 组件可替换 | 分别换 ASR/LLM/TTS 各挑最强 | 单一模型锁死 |
| 长对话成本 | 按组件计价 | 每轮重嚼整段音频历史，~10x 暴涨 |

**为什么写作助手选级联**：它的核心产物本就是**结构化文本**（整流后的书面句、派 worker 的任务 JSON、文章 Markdown）——正是级联中间天然就有的东西；它需要**意图路由/工具调用**（文本 LLM 主场）、**可审计复盘**（回看每级文字快照）、**分别优化**（中文 ASR 准 + 写作模型强）。而 S2S 的独门优势（情感、全双工闲聊）对"口述→成文"价值不大。

本项目的三阶段（整流/校准/记忆）正活在级联的**文字/LLM 层**里。**未来融合方向**：混合架构——秘书判定 intent 后，**闲聊走 S2S**（自然、有情绪、可打断），**任务/写作走级联**（结构化、可控、可审计），正好接上已有的意图路由层。

## 关键技术选择

- **语音全在浏览器**：Web Speech API 做 STT（`zh-CN`，实时中间结果 + 静音断句）+ SpeechSynthesis 朗读；MediaRecorder 录 **Opus/webm**（本身压缩，无需 ffmpeg）。零 API key。
- **LLM = DeepSeek** 走 Anthropic 兼容代理（`/v1/messages`，`x-api-key` + `anthropic-version` 头，raw-fetch 无 SDK）。三个 agent 全部 `thinking:{type:"disabled"}`——DeepSeek v4 默认开思考，关掉才低延迟、出干净 JSON、tight max_tokens 不被挤没。
- **存储 = Node 24 内置 `node:sqlite`**（零依赖）。四张表：`voice_chunks` / `calibrated_nodes` / `topics` / `tasks`。
- **前端 = React + Vite**，与后端同一个 Koa 进程（dev 挂 Vite 中间件 HMR）。

---

## 跑起来

```bash
cd audio-aura
npm install

# 环境变量（本容器 shell 里 ANTHROPIC_* 通常已就绪，可直接跑；否则参考 .env.example）
#   ANTHROPIC_BASE_URL  = https://api.deepseek.com/anthropic
#   ANTHROPIC_AUTH_TOKEN = <你的 DeepSeek key>
#   VOICE_MODEL_FLASH   = deepseek-v4-flash      （整流 + 意图路由）
#   VOICE_MODEL_PRO     = deepseek-v4-pro[1m]    （写作 worker）

npm run dev          # → http://127.0.0.1:8080 （PORT 可覆盖）
```

生产：`npm run build:web && npm start`。

## 两种验证方式

**A. 无麦克风（自测 / CI）——文本注入**
左下角「🧪 文本注入」把一句话当作语音识别结果喂进整条链路。试试：
- 闲聊：`今天天气不错啊` → 小语判 `💬 闲聊`，直接回应。
- 任务：`帮我把刚才聊的语音三阶段架构写成一篇技术博客` → 小语判 `🛠️ 任务`，派发写作 worker，右侧文章流式生成。

命令行冒烟：
```bash
curl -s -X POST http://127.0.0.1:8080/api/dev/inject-turn \
  -H 'content-type: application/json' -d '{"raw_text":"帮我把这个整理成周报"}'
```

**B. 真麦克风（真实体验）——需 Chrome**
1. 用 **Chrome / Edge**（Web Speech 仅 Chrome 系支持）打开转发端口的地址。
2. 点顶部 **🎤 开始说话**，授权麦克风，用中文口述。
3. 看：原文 → 整流后文本（左侧），小语的闲聊/任务判定；开 **🔊 朗读** 让小语语音回你。
4. 说一句"帮我把刚才说的整理成一篇……"，右侧写作 worker 出文章。
> 注：Web Speech 在 Chrome 上把音频送 Google 后端识别，需联网；无 `zh-CN` 语言包时顶部会提示，用方式 A 兜底。

## 自动化测试

```bash
npm test        # Playwright：注入闲聊+任务两类话，验意图路由 + 流式成文（用系统 Chromium）
```

---

## Rust 本地核心（演进方向）

为了实时性，热回路推理正从"远程大模型"迁往"本地小模型 + Rust 性能核心"。`native/` 是一个 napi crate（`audio-aura-native.node`），已落地 **M1：本地路由 SLM** —— 用 **Qwen3-1.7B**（GGUF Q8，mistral.rs，CPU）**一次调用完成"口语整流 + 意图路由"**，替掉原来两次远程 LLM 调用。推理跑在 libuv 线程池（napi AsyncTask），不阻塞 Node 事件循环。

```bash
# 一次性：装模型（~1.8GB，走 hf-mirror）
mkdir -p native/models && curl -L -o native/models/Qwen3-1.7B-Q8_0.gguf \
  https://hf-mirror.com/Qwen/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q8_0.gguf
# 编译 Rust 核心 → .node
(cd native && cargo build --release && cp -f target/release/libaudio_aura_native.so audio-aura-native.node)
# 启用本地路由（不设则默认走远程，TS 远程路径保留作验证/兜底）
VOICE_LOCAL_ROUTER=1 npm run dev
```

**延迟**：纯 CPU 上路由 ~8–19s/句（candle 朴素 CPU GEMM 未加速；MKL 与 f16 gemm 链接冲突，死路）。**GPU（sm_120）实测把路由打到端到端 ~450–500ms**（~20–30x），JSON 质量不变。

GPU 构建（一次性，RTX 5070 Ti / Blackwell sm_120，CUDA 13.2）：
```bash
# 1. 装精简 CUDA 13.2 dev 包（NVIDIA debian13 官方仓库；candle-core 用 cudarc 0.19.8，支持到 CUDA 13.3）
#    cuda-keyring → apt install cuda-nvcc-13-2 cuda-cudart-dev-13-2 cuda-nvrtc-dev-13-2 \
#                   libcublas-dev-13-2 libcurand-dev-13-2 libcufft-dev-13-2 libcusolver-dev-13-2 libcusparse-dev-13-2 cuda-cccl-13-2 cuda-crt-13-2
echo /usr/local/cuda-13.2/lib64 | sudo tee /etc/ld.so.conf.d/cuda-13-2.conf && sudo ldconfig
# 2. 编 GPU 版 .node（本 crate 的 cuda feature → mistralrs/cuda）
cd native && CUDA_PATH=/usr/local/cuda-13.2 PATH=/usr/local/cuda-13.2/bin:$PATH CUDA_COMPUTE_CAP=120 \
  cargo build --release --features cuda && cp -f target/release/libaudio_aura_native.so audio-aura-native.node
# 3. 跑（ldconfig 已注册库，无需 LD_LIBRARY_PATH）
VOICE_LOCAL_ROUTER=1 npm run dev
```
CPU 版：省略 `--features cuda` 与 CUDA 环境即可（慢但零依赖）。

**迁移路线**：M1 本地路由 SLM（✅ CPU 闭环）→ **M1-GPU（✅ 亚秒级路由）** → **M-transport（✅ Rust 独立守护进程脊柱）** → **M2 本地流式 ASR（✅ SenseVoice + VAD，接 omni-scout /audio）** → M3 本地流式 TTS（Kokoro/Piper）→ M4 完整流式级联 + 可打断（LiveKit 式重叠 + 语义断句）。选型与 LiveKit 调研见 `docs/livekit-port-notes.md`。

### Rust Stage1 本地 ASR（audio-aura-asr，M2 ✅）

`crates/audio-aura-asr`：`AudioChunk` + 能量 VAD（滞回状态机，照 livekit Silero 参数）+ `VadSegmenter`（livekit `StreamAdapter` 模式：批式识别器用 VAD 门控成流式）+ `Asr` trait。真实 ASR = **SenseVoice**（sherpa-onnx，中文 ~3% CER），`--features sherpa` 启用。`audio-aura-core` 的 `ingest.rs` 拉 **omni-scout `GET /audio`**（chunked 16k mono S16LE）→ 20ms 重帧 → VAD+ASR → 每句 Final 文本走现有 Stage2 pipeline。

**构建/运行**（sherpa 预编译 `.so` 放在工作区 `lib/`，二进制靠 `$ORIGIN`-relative RUNPATH 自动找到——**零系统 ldconfig、零 `LD_LIBRARY_PATH`**；crates.io 走 rsproxy 镜像）：
```bash
# 一次性：把 sherpa 预编译库链进工作区 lib/（版本无关的稳定门面）
ln -sf "$PWD/native/sherpa/sherpa-onnx-v1.13.4-linux-x64-shared/lib/libsherpa-onnx-c-api.so" lib/
ln -sf "$PWD/native/sherpa/sherpa-onnx-v1.13.4-linux-x64-shared/lib/libsherpa-onnx-cxx-api.so" lib/
ln -sf "$PWD/native/sherpa/sherpa-onnx-v1.13.4-linux-x64-shared/lib/libonnxruntime.so"        lib/
# 构建（RUNPATH 由 .cargo/config.toml 的 rustflags 自动烙入）
CUDA_PATH=/usr/local/cuda-13.2 PATH=/usr/local/cuda-13.2/bin:$PATH CUDA_COMPUTE_CAP=120 \
  cargo build -p audio-aura-core --release --features "cuda,asr"
SCOUT_AUDIO_URL=127.0.0.1:8100 VOICE_LOCAL_ROUTER=1 ./target/release/audio-aura-core   # 摄取 omni-scout 音频
```
> 原理：二进制烙入 `$ORIGIN/../../lib`（覆盖 `target/<profile>/` 下的 bin）+ `$ORIGIN/../../../lib`（覆盖 `examples/`），都解析到工作区 `lib/`；sherpa 三个 `.so` 自带 `RPATH=[$ORIGIN]`，传递依赖 `libonnxruntime.so` 在同目录自动解析。换 sherpa 版本只需重指 `lib/*.so` 软链，二进制和系统配置都不用动。
**已验证**：mock /audio（zh.wav）→ 摄取 → VAD 分段 → SenseVoice 转写"开放时间早上9点至下午5点"（84ms/句）→ Qwen 整流+路由 → chunk/node/secretary/SSE，全链路本地。SenseVoice 把"开放"听成同音"开饭"正好演示 Stage2 校准的价值。

### Rust 独立守护进程（audio-aura-core，M-transport ✅）

Rust 核心"脊柱"已立起：`crates/` workspace 四个 crate —— `audio-aura-router`(RouterEngine 纯逻辑 rlib，napi 与 daemon 共用) / `audio-aura-store`(rusqlite，与 `src/store.ts` 同 4 表、同 `data/voice-agent.db`) / `audio-aura-core`(axum 守护进程) / `audio-aura-asr`(Stage1 ASR)。daemon 在 `:9090` 暴露与 TS 后端**完全相同**的 API（`/api/stream` SSE + `/api/turn` + `/api/topics*` + `/api/chunks/:id/audio`），事件 JSON 照抄 `web/types.ts`，devtools-web 无感。

```bash
# 编 GPU 版 daemon（sm_120；CUDA 库已 ldconfig 注册）
cd audio-aura && CUDA_PATH=/usr/local/cuda-13.2 PATH=/usr/local/cuda-13.2/bin:$PATH CUDA_COMPUTE_CAP=120 \
  cargo build -p audio-aura-core --release --features cuda
# 起 daemon（自带 GPU RouterEngine + rusqlite + 远程 writer 兜底）
./target/release/audio-aura-core                      # → http://127.0.0.1:9090
# devtools-web 指向 daemon（前端仅展示）
VITE_API_BASE=http://127.0.0.1:9090 npm run dev  # SPA 在 8080，API 打到 9090
```

**已验证**：GPU 路由端到端 **~385–423ms**（比 napi-in-Node 更快）、SSE 事件流与 TS 一致、SQLite 落库正确、远程 writer 成文、真实浏览器里 SPA→daemon 全链路跑通。TS 后端(8080) 与 daemon(9090) 可并存、逐端点切换。目标形态"Rust 核心 + 前端纯展示"第一版达成。

## 目录

```
src/
  server.ts routes.ts service.ts     # Koa 入口 / REST+SSE 分发 / 管线编排(核心大脑)
  store.ts schema.sql audio.ts bus.ts# node:sqlite 存储 / 音频落盘 / SSE 事件总线
  config.ts  llm/deepseek.ts         # env / DeepSeek 客户端(非流式+流式)
  agents/ calibrator secretary writer registry
web/
  App.tsx  components/*  hooks/*      # 两栏 UI / 语音采集 / SSE 订阅
```

## 今天没做（结构已预留）

- 多个 worker（现只有 `write`；`agents/registry.ts` 是接入缝）、本地 Whisper 兜底 ASR。
- chat.txt 的"100 块滑动 + 自动话题切分"（现用手动「新话题」）。
- 剪贴板注入 / 全局热键、鉴权 / 多用户 / 云同步。
