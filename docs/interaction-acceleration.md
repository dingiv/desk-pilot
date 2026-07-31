# 用户交互加速

familiar (geek-familiar) 是一个常驻桌面的 AI 秘书前端。它通过三个交互通道加速用户的信息工作流：**语音识别**（将口语实时转写并纠偏）、**剪贴板**（跟踪系统剪贴板内容，提供缓冲编辑区）、**跨窗口拖拽**（从一个窗口拖拽内容到宠物进行处理）。

三个通道共享同一个数据流向：**感知 → 收集 → 呈现 → 派发**。

---

## 1. 语音识别 (ASR)

### 数据流

```
麦克风 → omni-scout 采集 → audio-aura daemon (VAD + ASR + LLM 校准)
  → SSE stream (/api/stream) → familiar (asr.rs)
  → ConversationTurn 存入历史 → Chat Panel 展示
```

### 参与者

| 组件 | 角色 |
|---|---|
| `omni-scout` | 音频采集前端（麦克风→PCM） |
| `audio-aura` | VAD（Silero）+ 流式 ASR（Zipformer）+ 批量 ASR（SenseVoice）+ LLM 校准（Qwen3 GGUF）→ 纠偏后的文本 + 意图分类（chat/task）+ LLM 回复 |
| `familiar` | SSE 客户端，解析 `interim`/`final` 事件，存入 `ConversationTurn` |

### SSE 事件类型

| 事件 | 含义 | 处理方式 |
|---|---|---|
| `hello` | 连接确认 | 日志 |
| `status` | scout 采集开关状态 | 更新录音按钮 `●`/`○` |
| `interim` | 流式部分结果（逐字符演进） | 最新一句话行内显示 `"— … —"` |
| `final` | 一句话完成（含 `calibrated`/`intent`/`reply`/`seq`） | 创建 `ConversationTurn`，清空 interim |
| `correction` | 用户纠正反馈 | 日志（未来：更新对应 turn） |

### 加速效果

- **减少了输入延迟**：口语转写是实时的，不需要手动打字再纠错
- **校准（calibrated）**：LLM 修正同音字/方言 → 比原始 ASR 准确
- **意图分类**：`chat` vs `task` → 决定是闲聊回复还是派发给后端 agent 执行
- **右键菜单**：任意 ASR 条目右键 → 复制到剪贴板缓冲编辑 → 派发 agent 或手动复制

---

## 2. 剪贴板 (Clipboard)

### 数据流

```
用户复制文字（任意应用）
  → GNOME Shell Extension (owner-changed 信号)
  → St.Clipboard.get_text() 读取内容
  → Unix socket 推送 {"type":"clipboard","text":"..."}
  → familiar (gnome_ext.rs subscribe_clipboard, 持久连接)
  → ClipboardUpdate → Vec<String> 历史 → Chat Panel 显示
           
备用（无扩展时）:
  → iced::clipboard::read() 每 3s 轮询 → ClipboardUpdate
```

### 参与者

| 组件 | 角色 |
|---|---|
| `gnome-layer-ext@desk-pilot` | GNOME Shell 扩展，监听 `MetaSelection::owner-changed`，用 `St.Clipboard.get_default().get_text()` 读取内容，通过持久 socket 推送给客户端 |
| `familiar (gnome_ext.rs)` | 持久 socket 客户端，订阅剪贴板变更；解析推送的 JSON，发送 `Message::ClipboardUpdate(text)` |
| `PetApp.clipboard` | `Vec<String>`，最多 50 条，按时间倒序，自动去重 |
| `Chat Panel` | 显示剪贴板历史（透明只读 `text_input`，可选中复制）+ ✏️ Buffer（可编辑 `text_input`，拼装文本） |

### 加速效果

- **消除窗口切换**：复制的内容自动出现在宠物面板里，无需切回
- **缓冲编辑（✏️ Buffer）**：可粘贴多条剪贴板内容，拼装组合后再复制出去
- **事件驱动（零轮询）**：扩展侧 `owner-changed` 信号触发，不消耗 CPU
- **降级兜底**：无扩展或非 GNOME 环境自动退到 3s 轮询 `iced::clipboard::read()`

---

## 3. 跨窗口拖拽

### 数据流

```
用户从其他窗口拖拽/选中文字 → （Phase 2 规划）
  → familiar 作为 drop target 接收 → 写入 ✏️ Buffer → 用户编辑 → 派发 agent
```

### 当前能力

| 功能 | 状态 |
|---|---|
| 宠物窗口可拖拽（compositor move） | ✅ `window::drag()`，通过 dock `⠿` 手柄触发 |
| 窗口大小可调（compositor resize） | ✅ `window::drag_resize()`，右下角 `⠶` grip |
| 跨工作区驻留 | ✅ GNOME 扩展 `win.stick()`，切换工作区宠物不消失 |
| 文件 drop（拖文件到宠物） | ⬜ Phase 2 |
| 文字 drop（拖文字到宠物） | ⬜ Phase 2 |

### 加速效果

- **随手可及**：宠物固定在所有工作区，不需要切窗口就能看到
- **缩小工作区切换成本**：随时语音、看剪贴板、派发 agent，不打断当前工作流

---

## 整体架构

```
┌────────────────────────────────────────────────────┐
│                   familiar (iced)                   │
│                                                    │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────┐ │
│  │ ASR Panel│  │Clipboard │  │  ✏️ Buffer       │ │
│  │ (语音结果)│  │ (剪贴板) │  │  (拼装编辑区)     │ │
│  └────┬─────┘  └────┬─────┘  └────────┬─────────┘ │
│       │              │                │            │
│  ┌────▼──────────────▼────────────────▼─────────┐  │
│  │              Service Layer                    │  │
│  │  asr.rs (SSE)  │  aura.rs (HTTP)  │ gnome_ext│  │
│  └────────────────────┬─────────────────────────┘  │
│                       │                             │
│  ┌────────────────────▼─────────────────────────┐  │
│  │              Model Layer                      │  │
│  │  ConversationTurn │ clipboard: Vec<String>   │  │
│  │  scratchpad       │ Message enum              │  │
│  └──────────────────────────────────────────────┘  │
└──────────────────────┬─────────────────────────────┘
                       │
         ┌─────────────┼─────────────┐
         ▼             ▼             ▼
    audio-aura    GNOME Shell    visual-rover
    (语音识别)    (剪贴板+置顶)   (agent 后端)
```
