# IME 输入法子系統 :设计文档

> **状态：设计阶段（2026-07-22）。** 本文档描述 desk-pilot 第四子系统 — 输入法增强引擎的架构设计和跨平台路线。

## 定位：秘书系统的"写"之手

desk-pilot 的五个子系统各司其职：

| 子系统 | 角色 | 一句话 |
|---|---|---|
| omni-scout | "看" | 屏幕 + 音频采集 daemon |
| audio-aura | "听" | 三阶段语音管线（ASR → 整流 → agent） |
| geek-familiar | "调度" | 常驻桌面精灵，秘书 UI + agent 调度 |
| **ime** | **"写"** | **输入法引擎：中英混输 + snippet 展开 + 语音插入 + 动作触发** |

IME 是秘书系统里"产出最终文本"的那只手 —— 语音听进来、意图路由完、最终要通过输入法把文字落进应用。

**物理形态：fcitx5 addon（`.so` 共享库）**，被 fcitx5 进程加载。Rust 业务逻辑在 `.so` 内起后台线程处理 SSE 连接(连 aura)和 TCP server(被 familiar 连)，`keyEvent` 路径上只做非阻塞的内存读写 —— 输入法引擎必须快（微秒级返回），所有可能阻塞的 I/O 都在后台线程。

## 核心功能

### 统一输入体验：一个引擎承载中英混输 + snippet + 语音

传统 Linux 输入法（ibus-pinyin / ibus-rime）的一个核心痛点：**编写中英文混杂文档时需要频繁 Super+Space 切换**——打中文切到拼音引擎，打英文切回 English(US)，每切换一次都是一次上下文打断。商业操作系统（macOS 内置中文输入法、Windows Microsoft Pinyin）的中英混输能力远强于 Linux ibus 生态，这正是我们要填补的空白。

我们的 IME 引擎**一次切换、五种输入模式全部可用**：

```
用户在当前引擎下键入任意字符：

  "n" → 拼音候选[你/呢/那]+ 英文缓冲[n] + snippet 检查[不匹配]
  "i" → 拼音候选[你/拟/尼]+ 英文缓冲[ni]+ snippet 检查[不匹配]
  "/" → 不是拼音前缀,放弃拼音路径;是 snippet trigger 前缀
  "g" → snippet 路径累积 "/g",检查 trie
  "r" → "/gr" 继续沿 trie 走
  "eet" → "/greet" 完整命中 → preedit 替换为展开文本

  按 Space → CommitText("你好,我是 AI 秘书...")
```

**状态机（三个并行路径）**：

```
                 ┌──────────────┐
   任何按键 ────→│   路由判定    │
                 └──┬───┬───┬──┘
                    │   │   │
         拼音合法?  英文?  snippet?
              │       │       │
              ▼       ▼       ▼
         PinyinPath  EnglishPath  TriggerPath
         (汉字候选)  (透传+累积)  (前缀匹配)
              │       │       │
              └───┬───┘       │
                  │           │
             CommitText    Expand+Commit
             (选定候选)    (展开文本)
```

**路由判定的规则（按优先级）**：

| 当前输入 | 判定 | 行为 |
|---|---|---|
| 以 `/` 开头 | snippet trigger 前缀 | 进入 TriggerPath,逐字沿 trie 匹配 |
| 以 `#` 开头 | 特殊命令(`#asr` / `#exec_`) | 进入 TriggerPath,匹配特殊触发器 |
| 纯拼音合法序列 | 中文输入 | 进入 PinyinPath,展示汉字候选窗 |
| 其他(数字/符号/非拼音英文词) | 英文直输 | 透传 CommitText(等效于 English/US 模式) |

**为什么这个架构天然适合我们的需求**：

1. **拼音引擎作为可选插件**：PinyinPath 委托给成熟的中文引擎（`chewing` crate 或 libpinyin）。Phase 1 不绑定具体拼音实现，架构上预留 `PinyinEngine` trait。拼音能力是**增量叠加**的——Phase 1 没有拼音时，PinyinPath 为空，引擎退化为纯 snippet expander + 英文直输。

2. **snippet 和拼音互不冲突**：`/` 不是拼音前缀，`#` 不是拼音前缀。用户打拼音时永远不会意外触发 snippet；打字面 `/greet` 时不会弹出拼音候选。

3. **#asr 语音插入在任意模式下可用**：无论用户正在打中文还是英文,`#asr` 都是明确的特殊触发——它是 `#` 前缀命令,不经过拼音引擎。

### snippets 展开

模仿 VS Code snippet 机制。用户输入预设触发词 → IME 替换为长文本。

```
键入: /greet → 展开为: 你好,我是 AI 秘书,请问有什么可以帮你的?
```

支持变量：`$DATE`（当前日期）、`$CLIPBOARD`（剪贴板内容）、`$CURSOR`（展开后光标位置）。

### #asr — 语音缓冲插入

```
键入: #asr → 读取 aura SSE 语音缓冲 → 替换为识别文本
```

IME 进程内维护一个持久 SSE 连接到 aura-daemon 的 `/api/stream`,累积 `AsrBuffer`（按 seq 组织的 streaming partial → final 文本）。`#asr` 触发时取最近 N 秒的 final calibrated 文本拼接。

### #exec — 关联动作触发

```
键入: #exec_resize_window → IME 发消息到 familiar → familiar 执行动作
```

安全边界：IME 只发触发消息，**不决定执行什么**——动作注册和执行在 familiar 侧完成。

### 配置联动：familiar 常驻、IME 瞬时

IME 候选窗口转瞬即逝。familiar 作为常驻桌面精灵，提供：
- snippets 规则增/删/改
- `#exec` 动作注册
- 语音缓冲区状态显示（长度、最后更新时间）
- 展开历史日志

familiar 配置变更后通过专用 socket **立即推送完整规则集**到 IME 进程 → IME 原子替换内存 SnippetStore → 下一次键入立即生效。

## 架构总览

```
┌────────────────── desk-pilot 系统边界 ──────────────────────┐
│                                                              │
│  任意 GUI 应用 ◄── commitString ──┐                          │
│                                   │                          │
│  ═══════════ fcitx5 进程内 ════════                          │
│                                   │                          │
│  ┌────────────────────────────────┼───────────────────────┐ │
│  │    fcitx5 核心                 │                       │ │
│  │                                ▼                       │ │
│  │  ┌────────────────── engine.so ──────────────────────┐ │ │
│  │  │ C++ thin glue (~130 行)                            │ │ │
│  │  │   keyEvent() → ime_core_process_key() ──┐          │ │ │
│  │  │   ← commitString() / updatePreedit() ←──┘          │ │ │
│  │  │                                                   │ │ │
│  │  │  ┌── ime-core-ffi (C ABI, cbindgen) ────────┐    │ │ │
│  │  │  │  ime_core_init / _process_key / _activate │    │ │ │
│  │  │  │  ime_core_deactivate / _reset / _select   │    │ │ │
│  │  │  └──────────┬───────────────────────────────┘    │ │ │
│  │  │             ▼                                      │ │ │
│  │  │  crates/ime-core (纯 Rust)         后台线程:        │ │ │
│  │  │   Matcher · Expander · Dispatcher  SSE → aura :9091│ │ │
│  │  │   SnippetStore · StateMachine       TCP ← familiar │ │ │
│  │  └───────────────────────────────────────────────────┘ │ │
│  └────────────────────────────────────────────────────────┘ │
│                                                              │
│  ════════ TCP :9601 ═══════════════════════════════════════  │
│                                                              │
│  ┌──────────────────────────────────────────┐               │
│  │  geek-familiar (常驻)                     │               │
│  │  SnippetsConfigPanel (egui)               │               │
│  │  ImeStatusOverlay                         │               │
│  └──────────────────────────────────────────┘               │
│                                                              │
│  ════════ HTTP/SSE :9091 ════════════════════════════════   │
│                                                              │
│  ┌──────────────────────────────────────────┐               │
│  │  aura-daemon                             │               │
│  │  GET /api/stream (SSE)                   │               │
│  │    interim {seq,partial,…}               │               │
│  │    final   {seq,calibrated,…}            │               │
│  └──────────────────────────────────────────┘               │
└──────────────────────────────────────────────────────────────┘
```

## crate 划分（依赖自下而上，无环）

```
crates/ime-core/            纯逻辑 crate，无 OS 依赖，可交叉编译 + 全量单测
  ├─ matcher.rs            前缀匹配 → 候选列表（trie，参考 espanso-match）
  ├─ expander.rs           选中 → 展开文本（变量替换：$DATE/$CLIPBOARD/$CURSOR）
  ├─ dispatcher.rs         中间件链：#asr / #exec / snippet 各一个中间件
  ├─ snippet_store.rs      从 JSON 加载，支持原子热替换
  └─ platform.rs           PlatformIme + PinyinEngine trait（跨平台适配器接口）

crates/ime-core-ffi/       cbindgen + cargo-c 构建的 C ABI 包装（给 fcitx5 C++ glue 用）
  ├─ cbindgen.toml         标注哪些 pub fn 导出为 C header
  └─ Cargo.toml            [lib] crate-type = ["cdylib"]

apps/ime/                  二进制 crate，Linux 主力
  ├─ main.rs               选后端 + 起 bridge + 初始化
  ├─ config.rs             ime.json 配置（snippets / 热键 / 通用设置）
  ├─ bridge.rs             SSE client（连 aura :9091）+ socket server（被 familiar 连 :9601）
  └─ backends/
       ├─ fcitx5/          C++ thin glue + CMakeLists.txt（fcitx5 addon，主力 Linux 后端）
       ├─ ibus.rs          #[cfg(target_os = "linux")] zbus + ibus DBus 引擎（兼容 Linux 后端）
       ├─ tsf.rs           #[cfg(target_os = "windows")] windows-rs TSF COM（Phase 4）
       └─ imk.rs           #[cfg(target_os = "macos")] objc2-input-method-kit（Phase 4）

社区依赖（cargo add，不手写）：
  inputx-pinyin              拼音 → 汉字候选（414K FST 词条，DP 全切分，9 种模糊音）
  inputx-scoring             贝叶斯候选排序（Q4 定点 log-space，n-gram 概率模型）
  inputx-l0                  用户学习（3 选自动 pin，硬重置友好）
  zbus                       Linux D-Bus 通信（ibus 引擎用，已在锁文件）
```

**`ime-core` 不依赖任何 OS API** — 和 `scout-drivers` 的 trait 设计同一个原则：平台适配器各写各的，`#[cfg(target_os)]` 条件编译，core 永远编译、全量单测。

## 核心抽象：`PlatformIme` trait

```rust
/// 平台适配器每一次按键的处理结果。
pub enum ImeAction {
    /// 无 trigger 匹配，原样透传。
    PassThrough,
    /// 正在输入中，显示预编辑文本（composition state）。
    Preedit(String),
    /// 弹出候选列表，等待用户选择。
    ShowCandidates { items: Vec<Candidate>, selected: usize },
    /// 提交最终文本到应用，替换 trigger 文本。
    Commit(String),
}

pub struct Candidate {
    pub text: String,      // 展开后的文本（截断用于候选栏显示）
    pub label: String,     // "snippet: /greet" | "asr" | "exec" | "history"
    pub preview: String,   // 候选栏显示文本
}

/// 跨平台 IME 适配器接口。
pub trait PlatformIme {
    /// 每收到一次按键调用一次。
    fn process_key(&mut self, key: KeyEvent, state: &mut ImeState) -> ImeAction;
    /// 用户选择了某个候选。
    fn select_candidate(&mut self, index: usize, state: &mut ImeState) -> ImeAction;
}
```

`ImeState` 持有当前累积的 typing buffer（trigger 前缀）、Matcher 引用、SnippetStore 引用。

## 跨平台策略

四平台 IME 底层完全不同 — 不存在统一的跨平台 IME 抽象库。对策：和 `scout-drivers` 完全相同的模式 — **平台各写一个适配器，共享 100% 的 `ime-core`**。

| 平台 | 目标框架 / API | 技术栈 | 协议 / 机制特点 |
|---|---|---|---|
| **Linux (Ubuntu)** | Fcitx5 (Addon) | Rust + C++ thin glue（`cbindgen` + `cargo-c`，参考 `fcitx5-cskk` 已验证路径） | 基于 Fcitx5 模块化插件机制，编译为 `.so` 由 fcitx5 进程加载，进程内直接调用，响应极快 |
| **Linux (Ubuntu)** | IBus (Engine) | Rust + `zbus`（已在锁文件） | 基于 D-Bus 进程间通信，输入法作为独立进程通过 IPC 交互。ibus-daemon 管理生命周期 |
| **Windows** | TSF (Text Services Framework) | Rust + `windows-rs`（COM 组件） | 注册为系统级 COM DLL 插入到各个应用进程中，`ITfTextInputProcessor` 等接口已有官方 Rust trait |
| **macOS** | Input Method Kit (IMK) | Rust + `objc2-input-method-kit`（ObjC FFI） | 基于 Apple 官方 AppKit/IMK 框架，以独立 `.app` bundle 形式注册 |

**Linux 双后端策略：fcitx5 主力，ibus 兼容。** 国产发行版(Ubuntu Kylin / Deepin / UOS / 银河麒麟)和中文社区教程已全面迁移 fcitx5，作为 Linux 主力适配目标。ibus 由 GNOME/Ubuntu 英文默认环境覆盖，DBus 协议简单（~300 行 zbus），紧随 fcitx5 之后完成。

### 社区轮子复用汇总

| 组件 | 来源 | 复用方式 | 许可 |
|---|---|---|---|
| **拼音引擎**（414K 词条、DP 全切分、9 种模糊音、用户学习） | [`inputx-pinyin`](https://lib.rs/crates/inputx-pinyin) + [`inputx-scoring`](https://lib.rs/crates/inputx-scoring) + [`inputx-l0`](https://lib.rs/crates/inputx-l0) | 直接 `cargo add`，生产级 MIT/Apache-2.0 | MIT/Apache-2.0 |
| **Trie 前缀匹配**（snippet trigger 检测） | `espanso-match`（参考算法） | 参考其 rolling trie 实现，自写 | GPL-3.0（仅参考） |
| **fcitx5 Rust addon 构建链** | `fcitx5-cskk`（参考 `cbindgen` + `cargo-c` 配置） | 参考其 Cargo.toml / cbindgen.toml，自写 thin glue | — |
| **跨平台 IME trait 面** | `imekit`（参考 API 设计） | 参考其 `InputMethod` trait + 事件枚举，自写 | MIT/Apache-2.0 |

### fcitx5 引擎接口：只需实现 1 个纯虚函数

`InputMethodEngineV2` 有且仅有一个 `= 0` 的纯虚方法需要实现，其余都有默认空实现：

```cpp
class ImeEngine : public fcitx::InputMethodEngineV2 {
    // ★ 唯一必须实现
    void keyEvent(const InputMethodEntry &entry, KeyEvent &keyEvent) override;

    // 可选覆盖（均有默认空实现）
    void activate(const InputMethodEntry &, InputContextEvent &) override;
    void deactivate(const InputMethodEntry &, InputContextEvent &) override;
    void reset(const InputMethodEntry &, InputContextEvent &) override;
};
```

`keyEvent` 内只做三件事：①调 `filterAndAccept()` 拦截按键、② `ime_core_process_key()` 调 Rust 状态机取回 `ImeAction`、③按 action 类型调对应的 fcitx5 API（`commitString`/`updatePreedit`/`updateUserInterface`）。

### UI 策略

fcitx5 的 UI 和引擎是分离的。引擎只通过 `ic->updateUserInterface()` 发数据，渲染由 UI addon 负责。

| Phase | 策略 | 说明 |
|---|---|---|
| **Phase 1** | fcitx5 内置 UI（classic-ui / kimpanel） | 零 UI 代码。候选窗 fcitx5 免费提供。唯一匹配不弹窗（snippet/`#asr`），多候选自动弹系统候选窗 |
| **Phase 2** | familiar 接管候选窗 | 候选数据走 :9601 socket → familiar 在透明 GTK4 悬浮窗用 egui 渲染。复用 familiar 已有渲染栈，不用写窗口管理/合成器协议 |
| **Phase 3**（可选） | 自写 fcitx5 UI addon | 实现 `UserInterfaceV2` 虚类（`updateInputPanel`/`updatePreedit`/`updateStatusArea`），完全自定义外观 |

```
crates/ime-core/              纯 Rust，平台无关，全量单测
       │
       │ (cbindgen: Rust pub fn → C header)
       ▼
crates/ime-core-ffi/          自动生成的 C ABI 包装（cbindgen + cargo-c → .so）
       │
       │ (C FFI: extern "C" fn)
       ▼
apps/ime/backends/fcitx5/     C++ thin glue（~100 行）
       │                       继承 fcitx5::InputMethodEngine
       │                       重写 keyEvent()/activate()/deactivate()/reset()
       │                       每个事件 → 调 ime_core_process_key()
       │                       commitString()/updatePreedit() 回调
       ▼
libime-fcitx5.so              由 fcitx5 加载的 addon 插件
```

参考：`fcitx5-cskk`（libcskk Rust 库 → cbindgen → cargo-c → C ABI .so → fcitx5-cskk C++ glue → Fcitx5 addon），已验证此构建链在生产环境可用。

### 要手写的 vs 复用的

| 层 | 手写量 | 说明 |
|---|---|---|
| `crates/ime-core/` | ~500 行 | Matcher(trie) + Expander(变量) + Dispatcher(中间件链) + SnippetStore(JSON 热加载) |
| `crates/ime-core-ffi/` | ~50 行配置 | `cbindgen.toml` + `Cargo.toml`（cargo-c），标注哪些 `pub fn` 导出为 C ABI |
| `apps/ime/backends/fcitx5/` | ~150 行 | ~100 行 C++ thin glue + ~50 行 CMakeLists.txt |
| `apps/ime/backends/ibus.rs` | ~300 行 | zbus DBus 服务，纯 Rust |
| `apps/ime/bridge.rs` | ~200 行 | SSE 客户端(连 aura) + TCP socket server(被 familiar 连) |
| `inputx-pinyin` | 0 行 | `cargo add` |
| `inputx-scoring` + `inputx-l0` | 0 行 | `cargo add` |

## 关键接口契约

### IME ↔ familiar socket（TCP localhost:9601）

IME 侧启动一个 TCP server，familiar 作为 client 连接。协议：JSON lines，每行一条消息，`\n` 分隔。

```
# familiar → IME（配置推送，触发即推送全量）
{"type":"config_push","snippets":[{trigger:"/greet",expand:"你好…",desc:"问候语"},…]}

# familiar → IME（查询请求）
{"type":"ping"}

# IME → familiar（状态通知）
{"type":"match_display","trigger":"/greet","preview":"你好，我是…"}
{"type":"expanded","trigger":"/greet","at":1721600000000}
{"type":"asr_used","text":"今天天气…","age_s":3.2}
{"type":"pong"}
```

### IME ↔ aura（HTTP/SSE，连 daemon :9091）

IME 启动时建立持久 SSE 连接到 `GET /api/stream`，在进程内维护 `AsrBuffer`：

```rust
struct AsrBuffer {
    // 按 seq 组织的未完成 partials
    partials: BTreeMap<u64, String>,
    // 最近 N 秒的 final 文本
    finals: VecDeque<FinalEntry>,
    max_age: Duration,  // 默认 60s
}

struct FinalEntry {
    seq: u64,
    text: String,      // calibrated 文本
    at: Instant,
}
```

SSE 事件处理：
- `interim{seq,partial}` → 更新 `partials[seq]`
- `final{seq,calibrated}` → 移除对应 partial，追加 `finals`，清理过期条目
- `#asr` 触发时 → 拼接 `finals` 中 `max_age` 内的 `calibrated` 文本

### Snippet 文件格式（`ime.json`）

```json
{
  "snippets": [
    {
      "trigger": "/greet",
      "expand": "你好，我是 AI 秘书，请问有什么可以帮你的？",
      "desc": "通用问候语"
    },
    {
      "trigger": "/sig",
      "expand": "Best regards,\n$CLIPBOARD\n$DATE",
      "desc": "邮件签名（含剪贴板内容）"
    }
  ],
  "triggers": {
    "asr": "#asr",
    "exec_prefix": "#exec_"
  },
  "asr_max_age_s": 60,
  "hot_capacity": 100
}
```

snippets 由 familiar 编辑后通过 socket 热推送；也可以直接编辑 `ime.json`（下次 IME 启动生效，或等 familiar 推送覆盖）。

## 实现阶段

| 阶段 | 内容 | 交付 |
|---|---|---|
| **Phase 1** | `crates/ime-core`（Matcher + Expander + Dispatcher + SnippetStore + PlatformIme trait）+ `crates/ime-core-ffi`（cbindgen C ABI）+ `apps/ime/backends/fcitx5/`（C++ thin glue ~130 行 + CMake） | fcitx5 addon `.so`，注册为系统输入法。snippets 展开可用（唯一匹配直接 commit；多候选走 fcitx5 内置候选窗）。拼音引擎预留 `PinyinEngine` trait（Phase 3 接 `inputx-pinyin`）。零 UI 代码 |
| **Phase 2** | ime-bridge（`.so` 内起后台线程: SSE client 连 aura + TCP server 被 familiar 连）+ familiar 侧 SnippetsConfigPanel + ImeStatusOverlay | familiar 配置面板可编辑 snippet 规则，即时推送 IME；`#asr` 语音缓冲插入可用 |
| **Phase 3** | 集成 `inputx-pinyin`（414K 词条、DP 全切分、9 种模糊音）+ `inputx-scoring` + `inputx-l0` | 中英混输完整可用（拼音、英文、snippet、#asr 全在一个引擎，不切换） |
| **Phase 4** | ibus DBus 引擎（`backends/ibus.rs`，~300 行 zbus） | Linux GNOME/Ubuntu 英文环境覆盖 |
| **Phase 5** | Windows TSF COM DLL（`backends/tsf/`）+ macOS IMK `.app` bundle（`backends/imk/`） | 跨平台输入法可用 |

Phase 1 全部业务逻辑在 `ime-core`（纯 Rust，容器内交叉编译无阻）。fcitx5 集成测试需要宿主机安装 `fcitx5` 开发包（`libfcitx5core-dev`），容器内只跑 `ime-core` 单测。

## 架构决策记录

**D1: 为什么是 .so 被 fcitx5 加载，而不是独立进程？**

fcitx5 输入法引擎本质就是 `.so` 插件，由 fcitx5 进程 `dlopen` 加载后直接函数调用（`keyEvent` → `ime_core_process_key`）。这带来了两个好处和一个约束：

1. **响应极快**：无 IPC 开销，`keyEvent` → Rust 状态机 → 返回 action 全程 <1µs（内存操作），因为不是独立进程所以不会被打断
2. **生命周期天然对齐**：fcitx5 启动我们就在，fcitx5 退出我们就卸载，不需要额外守护进程

**约束**：`keyEvent` 在 fcitx5 主线程上调用，必须快速返回。所有可能阻塞的操作（TCP/SSE I/O、文件写入）必须在 Rust 侧起的后台线程里做，`keyEvent` 路径上只用 `try_recv`（非阻塞）和 `Arc` 无锁读取。

**和 ibus 的关键区别**：fcitx5 是 `.so` 进程内调用，ibus 是独立 DBus 进程（`org.freedesktop.IBus.Engine`）。ibus adapter 是 Phase 4，fcitx5 优先。

**D2: 为什么 fcitx5 优先于 ibus？**

中国 Linux 发行版已全面迁移 fcitx5（Ubuntu Kylin / Deepin / UOS / 银河麒麟 / 中文社区教程）。ibus 是 GNOME/Ubuntu 英文默认，fcitx5 是中文用户主力。fcitx5 的 `.so` 插件模型和我们的 `.so` 内 IPC 架构天然匹配 —— Aurora SSE + familiar socket 都在后台线程，不打扰 fcitx5 主线程。

**D3: IME 直连 aura，不经过 familiar 中转。**

理由：延迟最低（snippet 展开要求毫秒级响应），且 familiar 可能没跑。familiar 只负责配置和可视化，不介入实时数据流 — 职责单一。

**D4: .so 内的 IPC — 我们做 server，familiar 做 client。**

IME 作为 fcitx5 addon（`.so`），**几乎永远在运行**（fcitx5 是用户会话级 daemon，随桌面启动）。familiar 是桌面精灵，用户可能随时关闭和重新打开。因此 IME 起 TCP server（:9601），familiar 作为 client 连接。familiar 连上 → 推送全量 snippet 规则 + 订阅事件流；familiar 断开 → IME 正常工作（用缓存规则 + #asr 缓冲区独立运行），等它重连时再推送最新状态。

**D5: fcitx5 .so 内起 Rust 后台线程的线程安全。**

`keyEvent` 在 fcitx5 主线程（C++ 侧回调），`ime_core_process_key()` 必须快速返回。架构上：
- 所有网络 I/O（aura SSE 客户端、familiar TCP server）在 `ime_core_init()` 时起的独立 Rust 线程
- `keyEvent` 路径上：`state.config_rx.try_recv()`（非阻塞，取 familiar 推送的配置）+ `state.asr_buffer.snapshot()`（`Arc` 无锁读取）+ `matcher.search()`（纯内存 trie 查找）
- Rust 侧的状态机（ImeState）用 `Mutex` 保护（锁持有时间 <1µs，只锁状态读写，不做 I/O）

**D4: 为什么要走真 IME 路线？—— Espanso TextExpander vs IME 候选词模式的本质差异**

Espanso 是 **TextExpander 模式**，我们这个 IME 是 **IME 候选词模式**。两者虽然产物相似（触发词 → 展开文本），但在操作系统层面完全是两套机制。理解这个差异才能看清为什么我们要走 IME 路线。

**Espanso 的做法（TextExpander）：**

```
用户键入 " / g r e e t "
  ↓ (每个字符正常进入应用——已经 commit 了)
应用收到: "/greet"
  ↓ (Espanso 的键盘钩子检测到匹配)
Espanso 模拟 6 次 Backspace 删掉 "/greet"
  ↓
Espanso 模拟键入展开文本的每个字符
  ↓
应用收到: "你好，我是 AI 秘书…"
```

四个阶段：**commit(触发词上屏了) → detect(钩子检测到) → delete(模拟 Backspace 删掉) → re-type(模拟键入展开文本)**。触发词**短暂出现在屏幕上然后被擦除**——这是 TextExpander 的本质特征。候选词概念完全不存在——只有"匹配到"和"没匹配到"。

**我们的 IME 做法（ibus 候选词模式）：**

```
用户键入 " / g r e e t "
  ↓ (ibus 拦截所有按键——尚未 commit)
ibus 引擎累积 preedit = "/greet" (应用看到下划线，尚未上屏)
  ↓ (Matcher 命中 /greet)
Preedit 直接替换为 expansion 或显示候选列表
  ↓ (用户确认/自动提交)
CommitText("你好，我是 AI 秘书…")
  ↓
应用收到: "你好，我是 AI 秘书…"
```

两个阶段：**intercept（拦截按键，不进应用）→ compose（preedit 显示触发词状态）→ commit（展开文本直达应用）**。触发词**永远不会出现在应用中**。

**关键差异对照表：**

| | Espanso(TextExpander) | 我们的 IME(ibus) |
|---|---|---|
| **按键流** | 按键直达应用,钩子偷看 | ibus 拦截按键,不被应用看到 |
| **触发词可见性** | 短暂出现在应用 → 被删掉(闪一下) | **从未出现在应用**——只在 preedit 中 |
| **候选词** | 无。有搜索栏(独立 UI),不是系统输入法 | **有。**ibus LookupTable,系统候选窗口,可多个 trigger 匹配后选 |
| **展开机制** | 模拟 Backspace 删 + 模拟键入 | 调用 CommitText 直接替换 preedit |
| **文本上屏** | 模拟按键逐字注入(N 次键盘事件) | 一次 IPC 提交 |
| **光标处理** | 手动计算 Backspace 次数 + 模拟方向键 | 框架自动处理 |
| **应用兼容性** | 部分应用不接受模拟按键(密码框/终端/远程桌面/Wayland) | **所有 GUI 应用**——ibus 是系统服务 |
| **中文场景** | 英文模式直输,不经过输入法管线 | 替换中文输入法当前引擎,可处理拼音+trigger 混合 |
| **视觉反馈** | 搜索栏弹出(需要自己画窗口) | 系统内联 preedit(下划线) + LookupTable 候选窗 |

**结论：**Espanso 的模式本质是**键盘宏**(keyboard macro)——监听键序列 → 模拟按键替换。我们的模式本质是**输入法引擎**(IME engine)——在操作系统文本输入管线中拦截并改写。IME 路线的原因：inline preedit 消除闪烁、系统候选窗口不再需要自己画、应用兼容性保证(尤其是密码框和 Wayland)。额外的复杂度(ibus/TSF/IMK 注册、preedit 状态机、候选列表管理)是值得的。Windows/macOS 如遇困难可退回 TextExpander 模式（D2）,但 Linux 首选 ibus 真 IME。

## Espanso 源码分析总结（2026-07-23）

Espanso 是最成熟的 Rust 跨平台 snippet expander（15 crate、13.7k stars、GPL-3.0）。阅读全部 15 crate 源码后，对我们 IME 设计的具体影响如下。

### 和我们同构的设计模式（验证了 IME 架构方向）

| 模式 | Espanso 做法 | 我们 IME |
|---|---|---|
| **叶子 crate 无 OS 依赖** | `espanso-match` 零平台依赖，纯 Rust | `ime-core` 同样，可交叉编译+全量单测 |
| **平台 trait + cfg** | `Source` trait + `get_source()` 按 `#[cfg(target_os)]` | `PlatformIme` trait + 同模式 |
| **引擎组装 crate** | `espanso-engine` 只依赖外部 crate，不依赖任何内部 crate | `ime-core` 只依赖 `tracing`/`serde`/`regex` |
| **中间件链** | 24 个 Middleware,`fn next(event, dispatch)` 可拦截/转换/追加事件 | #asr/#exec 做成中间件插入管道，不硬编码 if-else |
| **配置热更新** | 文件 watcher(notify crate) → 重启 worker 进程 | familiar socket 推送完整规则集 → 原子替换（更简单） |

### Espanso 匹配引擎：可直接复用

`espanso-match` 是纯 Rust、零平台依赖的匹配引擎。两种匹配器并行运行：

**Rolling Matcher（Trie 前缀树）**：`rolling/matcher.rs` + `rolling/tree.rs`。每个 key 触发将字符追加到累积缓冲区 → 沿 trie 走到匹配或失配。支持词边界匹配(`left_word`/`right_word`)、大小写不敏感(`chars_insensitive`)、方向键序列。匹配后有明确的 trigger 边界(从哪里开始替换、替换多长)。**这是 snippets 展开最核心的算法 —— 完全可以直接作为 `ime-core` 的匹配引擎基础。**

**Regex Matcher**：`regex/mod.rs`。`regex::RegexSet` 做多正则并行匹配，累积缓冲区后跑全量正则。支持命名捕获组作为变量。**适合复杂 trigger，但一般 snippets 用 trie 就够。**

对我们的影响：`ime-core::Matcher` 可以内置 trie + regex 双引擎（和 Espanso 一样），或者 Phase 1 只做 trie（覆盖 90% trigger），Phase 2 加 regex。

### Espanso 的中间件链：我们的 #asr/#exec 扩展点

`espanso-engine/src/process/default.rs` 里 24 个 Middleware 的注入顺序决定了语义。关键是:**匹配阶段和注入阶段被明确分离** — `MatcherMiddleware` 只产生 `MatchesDetected` 事件,`CauseCompensateMiddleware` 才负责删 trigger。这意味着 `#asr` 可以做成一个新中间件,在 `MatcherMiddleware` 之后注入:检测到 `#asr` → 发出 `AsrQuery` 事件 → 另一个 `AsrMiddleware` 读 AsrBuffer → 产生 `MatchSelected(text)` 事件走后续的正常渲染/注入管道。

### Espanso 不使用的地方（我们的 IME 需要不同方案）

**键盘事件源不同**：Espanso 用 `Source::eventloop()` 主动 hook 键盘(Win32 Raw Input / CGEventTap / XRecord / evdev `/dev/input/event*`)。**IME 是被动接收** —— ibus 调我们的 `ProcessKeyEvent`,TSF 调 `ITfKeyEventSink::OnKeyDown`,IMK 调 `handle(_:client:)`。Espanso 的 detect crate **我们不能复用**。

**文本注入方式不同**：Espanso 用 `Injector::send_string()` 模拟按键(SendInput/CGEventPost/uinput)。**IME 直接调用框架的 CommitText** —— 不需要模拟任何按键。Espanso 的 inject crate 只在降级到 TextExpander 模式时有用(Windows/macOS 兜底路径)。

**C FFI 不可取**：Espanso 的 detect 和 inject 后端使用 `#[link(name = "espansodetect")]` 和 `#[link(name = "espansoinject")]` 链接 C 库做实际的键盘 hook/注入。我们走纯 Rust:ibus 用 `zbus`(已在锁文件)、Windows TSF 用 `windows-rs`(微软官方维护)、macOS IMK 用 `objc2-input-method-kit`。

**多进程架构不需用**：Espanso 有 daemon→worker IPC(权限隔离 + 热更新通过杀 worker 重起实现)。我们是单进程 ibus 引擎 — 框架管理生命周期。familiar 配置热推送比重启进程更轻量(原子替换内存 store)。

### Espanso 的 Extension 系统：snippet 变量参考

`espanso-render` 的 `Extension` trait 定义了可组合的变量扩展:`{{clipboard}}`（剪贴板）、`{{date}}`（日期）、`{{random}}`（随机数）、`{{script}}`（执行脚本取 stdout）、`{{shell}}`（shell 命令）、`{{form}}`（表单输入）、`{{choice}}`（多选）。

我们的 `ime-core::Expander` 的变量系统可以完全对齐这些扩展类型 —— Phase 1 先做 `$DATE`、`$CLIPBOARD`、`$CURSOR`；后续叠加 `$SCRIPT`、`$SHELL`。

### 对 IME Phase 1 的具体影响

Espanso 分析后，`ime-core` Phase 1 的实现路径更明确了：

1. **Matcher**:直接实现 Rolling Trie(前缀树),参考 `espanso-match/src/rolling/`
2. **Expander**:变量系统参考 `espanso-render` Extension trait
3. **Dispatcher**:入口做 MiddlewareChain,`#asr`/`#exec`/snippet 各一个中间件
4. **SnippetStore**:配置加载参考 `espanso-config` 的 YAML→MatchStore 流程(我们改用 JSON,更简单)
5. **PlatformIme trait**:保持 trait 对象模式(`Box<dyn PlatformIme>`),但不用 Espanso 的 C FFI —— 纯 Rust

## 参考与相关

- [Espanso](https://github.com/espanso/espanso) — Rust 跨平台 Text Expander（13.7k stars），**架构同构**(平台 trait + 叶子 crate + 中间件链);`espanso-match` 的 Rolling Trie 可直接参考;inject crate 在 TextExpander 兜底路径复用
- [imekit](https://lib.rs/crates/imekit) — Rust 跨平台 IME 抽象库（支持 TSF/IMK/ibus）
- [objc2-input-method-kit](https://lib.rs/crates/objc2-input-method-kit) — macOS IMK framework 的 Rust 绑定
- [windows-rs ITfTextInputProcessor](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/UI/TextServices/struct.ITfTextInputProcessor.html) — Windows TSF 的 Rust COM 绑定
- desk-pilot 架构与设计原则见 `CLAUDE.md`、`docs/README.md`
- audio-aura 架构见 `docs/aura/architecture.md`
- geek-familiar 设计见 `docs/familiar/index.md`

- [Espanso](https://github.com/espanso/espanso) — Rust 跨平台 Text Expander（13.7k stars），trigger→expansion 架构可参考
- [imekit](https://lib.rs/crates/imekit) — Rust 跨平台 IME 抽象库（支持 TSF/IMK/ibus）
- [objc2-input-method-kit](https://lib.rs/crates/objc2-input-method-kit) — macOS IMK framework 的 Rust 绑定
- [windows-rs ITfTextInputProcessor](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/UI/TextServices/struct.ITfTextInputProcessor.html) — Windows TSF 的 Rust COM 绑定
- desk-pilot 架构与设计原则见 `CLAUDE.md`、`docs/README.md`
- audio-aura 架构见 `docs/aura/architecture.md`
- geek-familiar 设计见 `docs/familiar/index.md`
