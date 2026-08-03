# 连续转录文本区 — 跨 item 选择 / 复制设计

## 问题

当前每条 ASR 消息是独立的 `text_input` 卡片，文本选择被限制在单个卡片内。用户无法：

- 跨卡片拖选文字
- 一次性复制多句话的连续文本

## 根因

iced 的 widget 树中，每个 `text_input` 是独立的可聚焦控件。没有浏览器 DOM 中跨元素 Selection API 的等价物。要让选择跨越多个"消息"，必须让它们存在于**同一个控件**内。

## 方案：新增 Transcript 缓冲分区

在 ASR 分区上方增加一个"转录文本"分区，用单一的 `text_editor` 累积所有识别结果。

### 新增分区

```
┌─ 📜 Transcript ▼ ──────────────────────────────────┐
│ 🗣 今天天气怎么样  [10:30]                            │
│    ◀ 今天晴，25°C…                                  │
│                                                     │
│ 🗣 帮我写封邮件给老板请假  [10:31]                     │
│    ◀ 好的，邮件内容如下…                              │
│                                                     │  ← 自由选择，可跨行跨句
│ 🗣 再查一下明天的航班  [10:32]                        │
└────────────────────────────────────────────────────┘
─── divider ─────────────────────────────────────────
┌─ 💬 Messages ▼ ────────────────────────────────────┐
│ ... 保留原卡片视图（单条浏览、fix、play）              │
└────────────────────────────────────────────────────┘
 ```

### 数据模型

```rust
// PetApp 新增
transcript: text_editor::Content,
```

### SSE 事件处理

```rust
// AsrUpdate::Final 到达时：
let ts = chrono::Local::now().format("%H:%M");
let entry = format!(
    "\n🗣 {}  [{ts}]\n{}",
    turn.user_text,
    turn.reply
);
self.transcript.perform(text_editor::Action::Edit(
    text_editor::Edit::Paste {
        content: entry.into(),
        // 追加到末尾
    }
));
```

或者更简单的：用 `Content::with_text` 重建整个缓冲区（已有的 + 新的）。这样不需要用复杂的 Edit API。

### 交互行为

| 操作 | Transcript 区 | Messages 卡片区 |
|---|---|---|
| 鼠标拖选文字 | ✅ 跨行、跨句 | 单卡片内 |
| Ctrl+C 复制 | ✅ 选多少复制多少 | ✅ |
| 右键菜单 | 无（纯文本区） | 📋 Copy / ✏ Fix / 🔊 Play |
| 折叠/展开 | ✅ 可折叠标题栏 | ✅ |
| 高度调整 | ✅ 拖动 divider | ✅ |

### 文件变更

| 文件 | 变更 |
|---|---|
| `app.rs` | 新增 `transcript: text_editor::Content`，在 `AsrUpdate::Final` 中追加 |
| `model/mod.rs` | 无变更（transcript 是 text_editor 内部状态） |
| `view/chat.rs` | 新增 Transcript 分区（在 Messages 分区上方），与现有分区同构 |

### 优势

- **跨句选择**：一句话完整体现在一个 `text_editor` 内，选择自由
- **不破坏现有卡片**：Messages 分区保留，fix / play 交互不动
- **独立控制**：Transcript 和 Messages 各自折叠/展开，互不影响
- **实现简单**：text_editor 原生支持多行编辑+选择，只需追加文本

### 潜在问题

1. **text_editor 无 read-only 模式**：用户可能意外编辑转录文本。缓解：在 `Message::TranscriptEdit` handler 中丢弃编辑，始终用原文本覆盖。或者接受可编辑（灵活）。
2. **大文本性能**：长时间转录累积大量文本。缓解：限制缓冲区最多保留最近 50 条（~10KB），超出时移除旧条目。
3. **内容同步**：Transcript 区与 Messages 卡片数据同源（`asr.history`），需保证一致。

### 实现步骤

1. `app.rs`：加 `transcript` 字段，初始化为空 `Content`
2. `app.rs`：在 `AsrUpdate::Final` 处理中追加文本到 `transcript`
3. `app.rs`：限制 `transcript` 最多 50 条（通过移除旧文本）
4. `model/mod.rs`：新增 `Message::TranscriptAction(text_editor::Action)`（如果需要编辑拦截），或直接让 transcript 不可触碰
5. `view/chat.rs`：新增 Transcript 分区，与 ASR/Clipboard/Status 同构

### 不做的

- 不添加富文本（bold/italic/color）— 纯文本即可
- 不添加时间戳点击跳转到对应卡片 — 事后优化
- 不在 Transcript 上添加右键菜单 — 统一在 Messages 卡片上操作
