# PRIMARY vs CLIPBOARD 选择区分

## 背景

Linux 有两套独立的剪贴板机制：

| 机制 | Wayland/X 名称 | 触发方式 | 粘贴方式 |
|---|---|---|---|
| 鼠标选择缓冲 | `PRIMARY` | 鼠标拖选文字 | 中键粘贴 |
| 传统剪贴板 | `CLIPBOARD` | Ctrl+C 显式复制 | Ctrl+V |

当前 geek-familiar 只监听 `CLIPBOARD`（Ctrl+C），不区分两种来源。用户看到剪贴板历史时无法判断某条内容来自鼠标拖选还是显式复制。

## 数据流（现状 → 目标）

```
GNOME 扩展 (现状)
  Meta.Selection::owner-changed(SELECTION_CLIPBOARD)     ← 单一来源
  → push {"type":"clipboard","text":"..."}                ← 无来源标记

GNOME 扩展 (目标)
  Meta.Selection::owner-changed(SELECTION_PRIMARY)        ← 新增：鼠标拖选
  Meta.Selection::owner-changed(SELECTION_CLIPBOARD)      ← 已有
  → push {"type":"clipboard","source":"primary","text":"..."}
  → push {"type":"clipboard","source":"clipboard","text":"..."}
```

## Meta.Selection 类型常量

在 GJS (GNOME Shell) 中：

| 常量 | 值 | St.ClipboardType |
|---|---|---|
| `Meta.SelectionType.SELECTION_PRIMARY` | 0 | `St.ClipboardType.PRIMARY` |
| `Meta.SelectionType.SELECTION_CLIPBOARD` | 1 | `St.ClipboardType.CLIPBOARD` |

连接 `owner-changed` 信号时，回调的第二个参数就是 selection type。

## 需要修改的文件

### 1. `apps/geek-familiar/scripts/gnome-layer-ext@desk-pilot/extension.js`

**位置**：第 91-103 行，`owner-changed` 连接 + `get_text` 回调

**变更**：
- 用数组循环替代单一的 `SELECTION_CLIPBOARD` 连接
- 在回调中根据 selection type 确定 `source` 字符串
- 使用对应的 `St.ClipboardType` 读取正确 selection 的内容
- 推送 JSON 中新增 `"source"` 字段

伪代码：
```js
const SELECTIONS = [
    { type: Meta.SelectionType.SELECTION_PRIMARY ?? 0, source: 'primary', clipType: St.ClipboardType.PRIMARY },
    { type: Meta.SelectionType.SELECTION_CLIPBOARD ?? 1, source: 'clipboard', clipType: St.ClipboardType.CLIPBOARD },
];
for (const sel of SELECTIONS) {
    this._clipSelIds = [];  // 改为数组存储两个连接 ID
    const id = global.display.get_selection().connect('owner-changed', (_s, type, _src) => {
        if (type !== sel.type) return;
        this._clipboard.get_text(sel.clipType, (_c, text) => {
            const payload = JSON.stringify({
                type: 'clipboard',
                source: sel.source,
                text: text || ''
            }) + '\n';
            for (const conn of this._clipSubs) {
                try { conn.get_output_stream().write_bytes(new TextEncoder().encode(payload), null); }
                catch (_) { this._clipSubs.delete(conn); }
            }
        });
    });
    this._clipSelIds.push(id);
}
```

### 2. `apps/geek-familiar/src/model/mod.rs`

**变更**：
- 新增 `ClipItem` 结构体：
  ```rust
  #[derive(Debug, Clone)]
  pub struct ClipItem {
      pub text: String,
      pub source: String,  // "primary" | "clipboard" | "poll"
  }
  ```
- `PetApp.clipboard: Vec<String>` → `Vec<ClipItem>`
- `Message::ClipboardUpdate(String)` → `Message::ClipboardUpdate { text: String, source: String }`

### 3. `apps/geek-familiar/src/service/gnome_ext.rs`

**位置**：第 42-60 行，`subscribe_clipboard` 函数签名 + JSON 解析

**变更**：
- 回调签名：`FnMut(String)` → `FnMut(String, String)`（第二个参数是 source）
- 解析推送 JSON 时额外提取 `"source"` 字段，默认 `"clipboard"`（兼容旧扩展）
- 调用 `on_text(text, source)`

### 4. `apps/geek-familiar/src/app.rs`

**变更**：
- `clipboard_stream`（第 314 行）：回调传 `(text, source)` → `Message::ClipboardUpdate { text, source }`
- `clipboard_poll_stream`（第 323 行）：`iced::clipboard::read()` 无法区分来源 → 使用 `source: "poll"`
- `ClipboardUpdate` handler（约第 188 行）：
  - 创建 `ClipItem::new(text, source)`
  - 去重逻辑：仅比较 `text`，不比较 `source`（相同文字从不同来源复制仍算重复）

### 5. `apps/geek-familiar/src/view/mod.rs`

**位置**：第 141-148 行，剪贴板列表显示

**变更**：
- 每项前面显示来源图标：
  - `🖱` — `"primary"`（鼠标拖选）
  - `📋` — `"clipboard"`（Ctrl+C）
  - `🔄` — `"poll"`（轮询降级）
- 截断逻辑不变（60 字符）

## 降级策略

`iced::clipboard::read()` 底层通过 wayland `wl_data_device` 读取，但 winit 只暴露 CLIPBOARD。当 GNOME 扩展不可用时（非 GNOME 环境），轮询只能获取 CLIPBOARD 内容，标记为 `source: "poll"`。PRIMARY 的轮询在 winit 0.30 中不可行（wl_data_device 需要创建 data offer 并等待 selection 事件）。

## 验证

1. `cargo build -p geek-familiar --release` 编译通过
2. 在其他窗口 Ctrl+C 复制 → 宠物剪贴板显示 `📋`
3. 在其他窗口鼠标拖选（不按 Ctrl+C）→ 宠物剪贴板显示 `🖱`
4. 中键粘贴到别处 → 确认 PRIMARY 内容正确
5. 相同文字分别从 primary 和 clipboard 来 → 只显示一条（去重生效）
