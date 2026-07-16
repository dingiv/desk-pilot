# Desktop Pet（桌面精灵）· 计划文档

> 状态：**v0.11** · 日期：2026-07-16 · **`fs` crate（FileLoader）落地 + 皮肤运行时加载**
> 变更：v0.10→v0.11 引入 **`crates/fs`**（拷自 audio-aura/aura-fs，全量改名 `fs`：`[package.metadata.fs]`、`fs_ns.rs`、`fs::emit_namespaces()`；10 单测绿）——dev/prod 资产路径解析 + Cargo.toml 声明 namespace + `loader!()` 宏。**皮肤切运行时加载**：ui 声明 `SKIN = { dev = "assets/skins", prod = "~/.geek-familiar/skins" }`，`ui::skin_source("default/idle.png")` 启动时解析一次（缺文件兜底 `include_bytes!` 的 `IDLE_PNG`），`PetApp.skin` 持有 `ImageSource`；换皮肤不用重编译（为 §11 开放决策 #7 皮肤系统铺路）。**实测**：dev 路径 `skin: crates/ui/assets/skins/default/idle.png` ✅；改名 PNG 后 `bundled fallback` 仍正常渲染 ✅。注意：extern crate `fs` 与 `use std::fs` 会遮蔽——消费侧统一 `fs::` 全限定。
> 变更：v0.7→v0.10 完成 M2（输入穿透 FFI + compositor drag）、M3（egui 嵌入）、M3.5（声明式 `ui` crate）、M3.6（PNG + alpha 异形穿透）——细节见 §8 路线图与 §10 已解决项。
> 变更：v0.6→v0.7 装扩展到宿主 `~/.local/share/gnome-shell/extensions/gnome-layer-ext@vrover/` + relogin 让 Shell 发现；修两个 gjs 坑（`add_address` 要 `Gio.UnixSocketAddress.new(path)` 不是字符串；`write_all_async` 要 `TextEncoder().encode()` 不是字符串）；`GetExtensionInfo` state=1 无错、socket `/run/user/1000/gnome-layer-ext.sock` 起。**实测**：pet 连 socket → 扩展 `make_above` → pet 日志 `keep-above applied via gnome-extension`；开 1800×1300 不透明普通窗后 pet 仍在最上层（coral 12873 不变）。环境前提：容器需透传宿主 `/home/host`(=`/home/jiugui5209`) 和 `/run`；Shell 扩展只在启动时扫目录（新装的要 relogin 才发现），改扩展代码也要 relogin（模块缓存）。
> 变更：v0.5→v0.6 置顶架构翻转：扩展作 **socket 服务器**（`$XDG_RUNTIME_DIR/gnome-layer-ext.sock`），app **主动**带 token 请求；app 把 token 塞进窗口标题 `geek-familiar#<pid>`（无边框不可见），扩展按标题匹配 `Meta.Window` 再 `make_above()`（PID-namespace 无关）。`KeepAboveStrategy::enable(&PinRequest{app_id,token})`；`GnomeExtensionStrategy` 连 socket 发 JSON 读 `ok`。**实测**：mock server 收到 `{"v":1,"token":"76867","app_id":"org.vrover.GeekFamiliar"}`，app 日志 `keep-above applied via gnome-extension`。仅宿主内 `make_above` 待装扩展后验证。
> v0.4→v0.5：GNOME Shell 扩展初版（pull 扫 WM_CLASS，已弃用改 push）。
> v0.3→v0.4：搭起三平台窗口接口 `PetWindow`（透明+不规则输入区+置顶）+ Linux `KeepAboveStrategy`（layer-shell / gnome-extension）+ Win/Mac 后端 stub。新坑：gtk4-rs 0.11 未绑定 `gdk_surface_set_input_region`（见 §10）。
> v0.2→v0.3：完成 M1（GTK4 后端，实测透明窗 + 精确珊瑚圆）；**GTK4 已删 `set_keep_above`，GNOME/Wayland 无法用客户端 API 强制置顶（见 §10）**；crate 改名 `pet-*` → `core`/`render`/`platform`。
> 范围：本文件是项目**总入口**。子模块细节后续拆到 `docs/rendering.md`、`docs/platforms.md`、`docs/behavior.md`。

---

## 0. 愿景与目标

做一只常驻桌面的**桌面精灵**：透明、置顶、可交互、能动，**跨 Windows / macOS / Linux**，**先在 Linux Wayland 上跑通**。

**一句话定义**：跨平台透明置顶**画布覆盖窗口** + Rust 业务（行为状态机）+ C binding 平台层 + 可插拔渲染引擎（先调研、最坏自研）的程序化绘制桌面伴侣。

**本轮（M0）只做一件事**：把工程架子搭起来——Cargo workspace、core/render/platform 三层分离、零依赖可编译、headless 能渲一帧验证管线。不做美术、不接 GPU（留接口）。

**核心体验指标（北极星）**
- 透明 + 置顶 + 鼠标穿透（只点得到角色本体），三平台一致。
- 程序化绘制（本轮圆/矩形；后续接 GPU 渲染引擎，目标 dma-buf 零拷贝）。
- 行为自然：idle / walk / sleep / 互动（点击、拖拽）有反馈。
- 不抢焦点、全屏应用时自动退居背后。

**非目标（本轮）**
- X11、移动端、美术资产、AI 人格、Windows/macOS 的实际窗口实现（留 stub + 文档）。

---

## 1. 跨平台技术基线矩阵

立项矩阵（Win32 / Linux Wayland / macOS Cocoa）。结论：三平台在「透明 + 置顶 + 最大化保持置顶 + 全屏退居背后」一致；分歧在 **窗口容器、渲染对接 API、鼠标穿透粒度、代码控制位置、拖动方式**——这些正是「Rust 业务 + C binding 平台层」要各自封装的差异点。

| 技术维度 / 平台 | 🪟 Windows (Win32) | 🐧 Linux (Native Wayland) | 🍏 macOS (Cocoa/AppKit) | 统一抽象 |
|---|---|---|---|---|
| 底层核心后端 | 原生 Win32 + WGL | 纯 Native Wayland 协议 | 纯 AppKit (Cocoa) | 平台层各写 |
| 窗口管理容器 | 🚀 GLFW / SDL2 | 🚀 GTK4 窗口（无边框） | 🚀 手写 NSPanel 子类 | `PlatformBackend` trait |
| 自研渲染对接 | 直接绑定 OpenGL ES 上下文 | 注入 GTK4 GSK 纹理管线 | 注入 Metal 或 ANGLE 翻译层 | **`Renderer` trait + wgpu 候选（见 §5）** |
| 透明画布 (Alpha) | ✅ `WS_EX_LAYERED` 像素混合 | ✅ GTK4 Wayland Alpha | ✅ `self.isOpaque = false` | 平台各自开透明 |
| 非全屏常驻置顶 | ✅ `HWND_TOPMOST` | ⚠️ **GTK4 已删 `set_keep_above`**；Wayland 客户端无法强制置顶 | ✅ `[self setLevel:NSFloatingWindowLevel]` | GNOME 需 layer-shell（不支持）/扩展，见 §10 |
| 最大化时表现 | ✅ 保持置顶 | ✅ 保持置顶 | ✅ 保持置顶 | 一致 |
| 全屏化时表现 | ❌ 自动退居背后（独占激活链） | ❌ 自动退居背后（协议升级） | ❌ 自动退居背后（留原 Space，独占新 Space） | 一致；策略可配 |
| 非规则鼠标穿透 | ✅ `WS_EX_TRANSPARENT`（全局） | ✅ `gdk_surface_set_input_region`（像素级） | ⚠️ 动态 `ignoresMouseEvents` | trait 统一（macOS 需动态切换） |
| 代码控制窗口位置 | ✅ `SetWindowPos`（绝对坐标） | ❌ 无法控制（Wayland 协议限制） | ✅ `setFrame`（绝对坐标） | **见 §3 画布模型** |
| 用户拖动（移动） | 🚀 `WM_NCLBUTTONDOWN`（系统托管） | ⚠️ 自研画布内改宠物像素偏移 | 🚀 重写 `mouseDragged:` → `setFrame` | **见 §3** |
| 硬件帧同步时钟 | ⏱️ `wglSwapIntervalEXT(1)` | ⏱️ `GdkFrameClock` 回调 | ⏱️ `CVDisplayLink` | trait 统一 |

### 1.1 Linux 混成器差异（GNOME / KDE / wlroots）

Linux 内部三系在「透明 + 输入穿透 + dma-buf」上一致；**`keep_above` 在 GTK4/Wayland 已不可用**（GTK4 删除了该 API，见 §10）；差异只在**程序化移动窗口**（见 §3，画布模型已绕开）：

| 能力 | GNOME (Mutter) | KDE (KWin) | wlroots (Hyprland/Sway) |
|---|---|---|---|
| `wlr-layer-shell` 锚定 | ❌ Mutter 不实现 | ✅ | ✅ |
| 客户端设绝对坐标（标准 API） | ❌ | ❌ | ❌ |
| 程序化移动窗口（IPC） | ⚠️ 需 Shell 扩展 | ✅ KWin 脚本 | ✅ Hyprland/Sway IPC |
| 交互拖拽（`gdk_toplevel_begin_move`） | ✅ | ✅ | ✅ |

> 本机环境：**GNOME on Wayland**（`WAYLAND_DISPLAY=wayland-0`，已验证 live），Rust 1.96.0；**GTK4 4.20.1 已装**（M1）。

---

## 2. 关键架构决策

| # | 决策点 | 决定 | 说明 |
|---|---|---|---|
| D1 | 语言 | **Rust 业务 + C binding 平台层** | 业务（FSM/场景/配置/渲染高层）纯 Rust；窗口与图形上下文绑定各平台原生 C/ObjC 库（GTK4 / GLFW / Cocoa），经 FFI 接入。gtk4-rs / glfw / objc2 这类「C 库的 Rust 绑定」即「C binding」 |
| D2 | 窗口模型 | **画布覆盖窗口（Canvas Overlay）** | 一张大尺寸透明、置顶、无边框窗口作「画布」；精灵按画布内坐标绘制。详见 §3 |
| D3 | 鼠标穿透 | input region = 精灵本体形状 | 透明像素点击穿透；Linux 用 `gdk_surface_set_input_region`，Win32 用 `WS_EX_TRANSPARENT`，macOS 动态 `ignoresMouseEvents` |
| D4 | 渲染 | **可插拔 `Renderer` trait**；本轮零依赖 CPU 基线画圆/矩形 | 具体 GPU 引擎经 §5 调研后再选；CPU 基线同时是最坏情况自研的雏形 |
| D5 | 行为 | 纯客户端 FSM（无 AI） | AI 人格后续可选层 |
| D6 | 定位/移动 | **画布内坐标偏移**（不移动窗口） | 绕开 Wayland 定位限制；Win/macOS 可选原生窗口移动作为优化 |
| D7 | 线程 | 主线程跑事件/帧循环；重活（解码/纹理上传）放工作线程 | 避免在 draw 回调里做重活 |

---

## 3. 「画布覆盖」窗口模型（Crux）

矩阵里两个 ⚠️ 都指向同一个根因：**Wayland 不让客户端控制窗口绝对坐标**。解法不是去对抗混成器，而是**换坐标系**——不移动窗口，移动窗口内的精灵。

**模型**
- 创建一张**大尺寸透明置顶无边框窗口**（覆盖工作区/多显示器，或动态裁剪到精灵周围+边距）作为「画布」。
- 精灵的「位置」= 画布内坐标 `(x, y)`。**拖拽 = 改坐标；随机漫步 = 定时改坐标；点击反应 = 换形态**。全程不调用任何「移动窗口」API。
- 输入穿透只保留精灵本体的 input region，其余区域点击直接落到下层应用。
- 全屏应用时，画布窗口照常被 Wayland「退居背后」（协议级行为，免费获得）。

**为什么三平台都吃这套**
- Linux/Wayland：唯一能在 GNOME 上实现「自动走动」的通用方案（不依赖 layer-shell / Shell 扩展）。
- Windows / macOS：画布模型同样可行；此外它们**额外**能用 `SetWindowPos` / `setFrame` 原生移动小窗口（OS 集成更好、合成开销更低）。作为后续优化项，不是本轮。

**代价/待解**
- 大透明层要被合成器每帧合成 → 优化方向：按精灵包围盒裁剪窗口尺寸；或 Win/macOS 切原生小窗口移动。
- 多显示器：画布需跨屏，或每屏一张画布。

> 结论：把「窗口定位」降级为「画布内绘制坐标」，三平台共用一套 `PlatformBackend` 抽象，Linux 不再有功能性短板。

---

## 4. 技术栈：Rust 业务 + C binding 平台层

**Rust（业务 + 编排）**
- `core`：场景/几何/行为 FSM/配置 —— 零平台依赖，全平台共用。
- `render`：`Renderer` trait + CPU 基线；GPU 引擎经 §5 选定后在此插入。
- `platform`：`PlatformBackend` trait + 各平台后端（feature-gated）。
- `geek-familiar`：二进制，按平台/feature 装配。

**C binding（平台原生窗口/图形）**
| 平台 | 窗口容器 | 图形上下文 | Rust 绑定 |
|---|---|---|---|
| Linux | GTK4 无边框透明窗口 | GTK4 `GtkGLArea`（直接自渲染）或 纹理→GSK（dma-buf 零拷贝） | `gtk4-rs`（C GTK4 的绑定） |
| Windows | GLFW/SDL2 | WGL / OpenGL ES | `glfw`/`sdl2` + `glow`/`gl` |
| macOS | 手写 `NSPanel` 子类 | Metal / ANGLE | `objc2` + `metal`/`core-graphics` 或 wgpu 的 Metal 后端 |

**图形 API 统一的关键洞察**：矩阵「自研渲染对接」一行三平台分别是 OpenGL ES / GSK / Metal-ANGLE。若选 **wgpu** 做 GPU 渲染，它用一套 Rust API 把这一行折叠成统一接口（Win→DX12/GL、Linux→Vulkan/GL、Mac→Metal）。这正是 §5 把 wgpu 列为首选候选的理由。

---

## 5. 渲染引擎策略：先调研，最坏自研

**前提**：要「在透明、非规则窗口上程序化绘制 + 跨平台 + 能对接各平台合成器」的现成 UI 框架，市面没有开箱即用的。计划：**先在 Rust 生态调研**，能复用就复用；不行再做最坏的「自研简易引擎」。

**候选调研（待 spike 验证）**
| 候选 | 类型 | 跨平台 | 适合度 | 备注 |
|---|---|---|---|---|
| **wgpu** | GPU 抽象（Vulkan/Metal/DX12/GL） | ✅ 三平台 | ★★★ 首选 | 一套 API 折叠三平台图形行；Linux 可渲染到纹理注入 GSK |
| **femtovg** | GPU 2D 矢量（OpenGL/NanoVG 风） | ✅（需 GL） | ★★★ 适合圆/矩形/路径 | 专为 2D 形状；本轮需求最贴 |
| **vello** | GPU compute 2D（wgpu 上层） | ✅ | ★★ | 现代，但偏重 |
| **skia-safe** | Skia 绑定 | ✅ | ★★ | 强大但体积大 |
| **tiny-skia** | 纯 Rust CPU 2D 光栅 | ✅ 无系统依赖 | ★★ 基线/最坏保底 | 无 GPU，但跨平台、能画圆/矩形、抗锯齿 |
| **ash/vulkano** | Vulkan 原生 | ✅（mac 需 MoltenVK） | ★ | 偏底层 |
| **自研** | 手写最小光栅/扫描线 | ✅ | — | 最坏情况；本轮 CPU 基线已是雏形 |

**本轮动作**：`render` 提供 `Renderer` trait + **零依赖手写 CPU 基线**（填充矩形 + 中点画圆），证明管线通；具体 GPU 引擎在 `docs/rendering.md` 的 spike 里定。

---

## 6. 工程结构与脚手架（本轮交付）

```
geek-familiar/
├─ Cargo.toml                 # workspace
├─ .gitignore
├─ docs/
│  ├─ index.md                # 本文件
│  ├─ platforms.md            # 待写：三平台矩阵 + 各后端细节
│  ├─ rendering.md            # 待写：渲染引擎 spike 结论
│  └─ behavior.md             # 待写：FSM/行为脚本
├─ scripts/
│  └─ gnome-layer-ext/      # GNOME Shell 扩展（uuid gnome-layer-ext@vrover）：socket server → Meta.Window.make_above()（Shell 49）
└─ crates/
   ├─ core/               # 纯 Rust：Scene/geometry/FSM/config/Canvas（零依赖）
   ├─ fs/                 # FileLoader：dev/prod 资产路径解析 + Cargo.toml namespace + loader!() 宏（拷自 audio-aura/aura-fs）
   ├─ render/             # Renderer trait + 零依赖 CpuRenderer（画圆/矩形）
   ├─ platform/           # PlatformBackend + App + PetWindow trait；后端 feature-gated
   │  ├─ window.rs        #   跨平台 PetWindow（透明 + 不规则输入区 + 置顶）+ InputRegion / KeepAboveMode
   │  ├─ event.rs / headless.rs  # 归一化事件 + headless（默认，渲一帧→PPM，零系统库）
   │  ├─ gtk.rs           #   GTK4 后端（feature=gtk）：透明窗口 + MemoryTexture 上屏 + GtkPetWindow
   │  ├─ keep_above.rs    #   Linux 置顶策略 trait：LayerShell / GnomeExtension（feature=gtk）
   │  ├─ windows.rs       #   Windows 后端 stub（GLFW/WGL，feature=windows，M5）
   │  └─ macos.rs         #   macOS 后端 stub（NSPanel/Metal，feature=macos，M5）
   └─ geek-familiar/            # 二进制：装配 App + Renderer + 选定后端
```

**分层契约（解耦关键）**
- `PlatformBackend` 不认识渲染器：每帧把一块 `Canvas`（RGBA8 buffer）交给 `App::render(&mut Canvas)`，App 内部用自己的 `Renderer` 画；平台只负责「开窗 + 帧时钟 + 把 buffer 上屏 + 事件回投」。
- **窗口属性走 `PetWindow` trait**：透明、不规则输入区（只点得到本体）、置顶，三平台统一接口；Linux/GTK 由 `GtkPetWindow` 实现，Win/Mac 是 stub。
- **Linux 置顶走 `KeepAboveStrategy` trait**（`keep_above.rs`）：`enable(&PinRequest{app_id, token})`。`GnomeExtensionStrategy`（GNOME）= **push**：app 把 token 放窗口标题 `geek-familiar#<pid>`，连 `$XDG_RUNTIME_DIR/gnome-layer-ext.sock` 发 JSON，扩展（`scripts/gnome-layer-ext/`，socket 服务器）按标题匹配窗口 `make_above()`；客户端/协议已对 mock 实测通过。`LayerShellStrategy`（wlroots/KDE）仍 stub。
- 这样换平台不动业务、换渲染器不动平台、换置顶策略不动窗口。

---

## 7. 行为与交互模型

**FSM（纯客户端，画布坐标）**
```
idle ──(timer)──> walk ──(edge)──> idle
  │(click)                  │(sleep timer)
  ▼                         ▼
react ─> idle             sleep ──(wake)──> idle
  ▲
  │(pointer down + move)
drag（改画布内 pet.pos） ──(release)──> idle
```
- 事件源：平台回投的 Pointer 事件 + 帧时钟 dt。
- 位置全在画布坐标系；拖拽直接写 `pet.pos`，不碰窗口。
- 本轮：idle 小幅 bob + 缓慢漂移 + 拖拽，验证状态机骨架。

---

## 8. 分阶段路线图

| 阶段 | 状态 | 目标 | 交付物 | 验证 |
|---|---|---|---|---|
| **M0 ✅** | done | 工程架子 | workspace + core/render/platform + headless 渲一帧 | `cargo build` 绿；PPM 含圆 ✅ |
| **M1 ✅** | done | Linux 透明画布窗 + 置顶 | GTK4 无边框透明窗 + `MemoryTexture` 每帧上屏；GNOME Shell 扩展 `gnome-layer-ext@vrover` push 模型 `make_above()` | GNOME 实测透明窗 + 精确珊瑚圆；keep-above 压在普通窗之上 ✅ |
| **M2 ✅** | done | 拖拽 + 不规则点击穿透 | (a) `gdk_surface_set_input_region` FFI + cairo::Region（透明区穿透）; (b) compositor drag `gdk_toplevel_begin_move` FFI（xdg-toplevel move 全屏拖拽）; (c) FSM drag（画布内偏移，备用路径）; (d) GNOME `skip_taskbar`+`skip_pager` ghost hints（扩展内）| 透明区穿透 ✅；全屏拖拽 ✅；Activities Overview 排除 ✅（待 relogin 确认）|
| **M3 ✅** | done | egui 渲染引擎（嵌入）| **egui 0.28** headless 嵌入：offscreen wgpu → RGBA8 readback → `MemoryTexture` → Picture；NVIDIA Vulkan (RTX 5070 Ti)；`egui` feature 门控；需求驱动渲染（idle CPU 0.5–2.25%）| GNOME 实测 egui UI 渲染 + 交互 ✅ |
| **M3.5 ✅** | done | 声明式 UI 层（Flutter/Elm）| **`ui` crate**（View enum: Text/Button/TextEdit/Circle/Image/Column/Row/Container/SizedBox + Msg + `column![]` 宏 + `.color()/.size()/.padding()` 链式）；**egui binder**（`egui_view.rs`：render_view→Msgs）；**Elm 模型**（`App::view()→View` + `App::update(Msg)`）；GTK4 事件桥接（Pointer hit-test 拖拽vs点击 / Motion hover / Keyboard → egui RawInput）| PetApp 声明式 UI + 按钮/文本/拖拽交互 ✅ |
| **M3.6 ✅** | done | 图片 + 透明主体 + 异形穿透 | **`View::Image`**（bundled `include_bytes!` PNG → Lanczos3 CPU 预缩放 → NEAREST 1:1 = 清晰不糊；`ImageSource::Path/Bytes`）；透明 `CentralPanel`（Frame fill=TRANSPARENT）；**alpha-derived click-through**（扫描 RGBA8 alpha → 扫描线 rects → input region = 精灵实际轮廓，非包围盒）| 用户 `idle.png`（1088×1088 不规则形）实测异形窗 ✅ |
| **M4** | next | 零拷贝（活动渲染 CPU 优化）| `GtkGLArea` + egui-wgpu 直接渲染到窗口 surface（消除 GPU→CPU→GPU 往返 readback）| 活动渲染 CPU 从 ~11% 降至 ~1% |
| **M5** | future | 跨平台后端 | Windows（GLFW/WGL）、macOS（NSPanel/Metal）；macOS `ignoresMouseEvents` 逐像素穿透 | Win/mac 实测 |

---

## 9. 环境与运行

- [x] Wayland 会话：`WAYLAND_DISPLAY=wayland-0` ✅
- [x] Rust 工具链：`rustc 1.96.0` ✅
- [x] M0 零依赖 headless：本机直接 `cargo run -p geek-familiar` ✅（无需 GTK4）
- [x] **GTK4**：`libgtk-4-dev` 4.20.1 已装 ✅
- [x] GTK4 后端：`cargo run -p geek-familiar --features gtk` 已验证透明窗 + 圆 ✅

---

## 10. 风险与未知

| 风险 | 影响 | 缓解 |
|---|---|---|
| 画布大透明层合成开销 | CPU/GPU 占用 | **实测**：idle 0.5%（需求驱动）；hover 2.25%（Motion 事件触发）；活动渲染 ~11%（GPU→CPU 回读管道，M4 解决）|
| 多显示器画布跨屏 | 错位/裁切 | 每屏一画布 or 跨屏单画布（**推迟**——当前单显示器验证） |
| dma-buf / GLArea 零拷贝 | M4 活动渲染 CPU | 先跑通 readback 路径（M3 ✅）；M4 切 `GtkGLArea` + egui-wgpu 直渲到 surface |
| ~~GTK4/Wayland 无置顶 API~~（**已解决 v0.7**）| GNOME `make_above()` via Shell 扩展 | GNOME ✅；wlroots/KDE layer-shell 待接 |
| ~~gtk4-rs 0.11 未绑定输入穿透~~（**已解决 v0.8**）| `gdk_surface_set_input_region` FFI + cairo::Region | ✅ 实测 |
| ~~gtk4-rs 0.11 未绑定 `begin_move`~~（**已解决 v0.9**）| `gdk_toplevel_begin_move` FFI（xdg-toplevel move）| ✅ 全屏拖拽实测 |
| ~~鼠标穿透形状不准~~（**已解决 v0.10**）| 不再用程序化圆形扫描线——直接从**渲染 RGBA8 的 alpha** 生成扫描线 region（精灵实际轮廓，支持任意不规则 PNG）| ✅ 用户 `idle.png` 异形穿透实测 |
| ~~无现成 UI 框架~~（**已解决 v0.10**）| **egui 0.28** headless 嵌入（render-to-buffer）+ **`ui` crate** 声明式 View/Msg | ✅ 声明式 UI + 图片 + 异形穿透实测 |
| egui 0.28 纹理无 mipmap | 高清 PNG 缩放可能锯齿 | CPU Lanczos3 预缩放到显示尺寸 + NEAREST 1:1 渲染 = 清晰（已解决）|
| GPU→CPU→GPU 回读管道 | 活动渲染 CPU ~11% | M4：egui-wgpu 直渲到 `GtkGLArea` surface（消除回读）|
| GNOME skip_taskbar/skip_pager | Activities Overview 排除 | 扩展内设置（property + method fallback），已部署待 relogin 确认 |
| macOS 穿透需动态切 `ignoresMouseEvents` | 频繁切换抖动 | M5：按渲染 alpha 逐像素决定（同 Linux 的 alpha-derived 逻辑）|
| macOS 穿透需动态切 `ignoresMouseEvents` | 频繁切换抖动 | 按 hover 区域批量切，非每像素 |
| 三平台图形行差异 | 渲染后端各自适配 | wgpu 折叠；Renderer trait 隔离 |
| 无现成框架满足全部需求 | 渲染要自研 | §5 调研优先；CPU 基线即最坏雏形 |

---

## 11. 待确认的开放决策

1. ~~**GPU 渲染引擎**~~（**已定 v0.10**）：选定 **egui 0.28**（headless 嵌入 render-to-buffer）；声明式 UI 层 = `ui` crate（View/Msg，Flutter/Elm-style）。
2. **画布 vs 原生小窗**：Linux 用画布覆盖模型 + compositor drag（`begin_move`）；Win/macOS M5 再定。
3. ~~**多显示器策略**~~（**推迟**）：当前单显示器验证。
4. ~~**角色形态**~~（**已定 v0.10**）：高清 PNG 大图 + 透明底板（`View::Image`，bundled via `include_bytes!`）；不同形态 = 不同 `src`（用户保证素材对齐）。
5. **是否引入 AI 人格层**（后续可选，接 GLM/本地模型）——是否现在就预留对话/事件接口？→ 倾向是（voice-agent 已有 intent-route）。
6. ~~**GNOME 自动移动**~~（**已定**）：compositor drag 解决（`begin_move`）；不需 Shell 扩展做移动。
7. **外观自定义**：View 纯数据 → 可 serde 序列化 → 用户加载皮肤/主题（皮肤目录 = `assets/skins/<name>/`）。**待做**。

---

## 12. 参考资料

- GTK4 / GSK：`GtkSnapshot`、`TextureNode`、`GtkGLArea`、`DmabufTextureBuilder` — https://docs.gtk.org/gtk4/
- gtk4-rs：https://gtk-rs.org/gtk4-rs/stable/latest/docs/
- 鼠标穿透：`gdk_surface_set_input_region`；Win32 `WS_EX_TRANSPARENT`；macOS `ignoresMouseEvents`
- Wayland 定位约束：`gdk_toplevel_begin_move`、wlr-layer-shell（GNOME 不支持）—— 画布模型绕开
- 渲染候选：wgpu / femtovg / vello / skia-safe / tiny-skia
- 复用经验：本仓库 visual-rover（GNOME/Wayland、Rust 工作区）— 见 memory `native-driver-layer`
