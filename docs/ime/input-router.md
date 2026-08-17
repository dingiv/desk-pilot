# 输入路由层 —— StateMachineTable(状态机表)

> 2026-08-17 · 代码为准:`crates/ime-core/src/router.rs`

## 为什么要有这一层

重构前,按键语义分散在三处,而且引擎**看不到修饰键**:

1. C++ `swift-ime.cpp keyEvent` 自行拦截 Ctrl/Alt 组合(直接放行)、把方向/
   翻页/`[`/`+`/`-` 映射成魔法 int 走独立入口;
2. `engine.predict_ctx` 又从 char 反猜特殊键(`' '`→Space、`'1'-'9'`→Digit);
3. `special_key.rs handle_special_key` 实际处理(面板门控、导航、翻页、选词)。

后果是同一引擎两副面孔:fcitx 里 `-` 翻页、TUI/mock 里 `-` 是终止符提交;
`InputEvent.ctrl/shift` 字段从未被任何前端填过 —— Ctrl+/ 这类策略根本进不了
引擎,只能在 C++ 里打补丁。

## 架构

```
前端(fcitx5 C++ / TUI / mock)
   │  忠实转换:键 + Ctrl/Shift/Alt 状态(不做任何拦截)
   ▼
KeyEvent { kind: KeyKind, ctrl, shift, alt }
   │
   ▼
┌──────────────────────────────────────────────┐
│ StateMachineTable(状态机表,每输入上下文一张)│
│  flags: COMPOSING | PANEL_OPEN | PINYIN |    │
│         SNIPPET | MAGIC | WORD_BUILDING |    │
│         PENDING                              │
│                                              │
│  route(sm, key, env) → ImeView + action 位   │
└──────────────────────────────────────────────┘
   │ 驱动迁移               │ 返回
   ▼                        ▼
StateMachine(组合状态机)   action: HANDLED / PASSTHROUGH / COMMIT
```

外界调用者**只按 action 反应**:

- fcitx5:`action & HANDLED == 0` → 不 `filterAndAccept`,键自然到达应用;
- TUI:`COMMIT` → 追加历史;TUI 自身作为"应用",对 PASSTHROUGH 的
  Ctrl+Q / Ctrl+C 退出。

## 状态标志位(StateFlags)

| 标志 | 含义 |
|---|---|
| `COMPOSING` | 组合中(拼音/词组/snippet/magic 任一) |
| `PANEL_OPEN` | 候选面板展开 |
| `PINYIN` / `SNIPPET` / `MAGIC` | 组合模式(对应 ComposeState) |
| `WORD_BUILDING` | 自生词模式:`committed_text` 非空(已逐字提交) |
| `PENDING` | 有待确认的展开候选(pending expansion / magic hints / preview tail) |

每次按键路由后从 `StateMachine` 重新同步(`sync_from`);路由之外修改 sm 的
入口(选词、reset、magic tick)也会同步 —— 表永远是当前状态的镜像。
`engine.state_flags()` 可随时查询(TUI 状态栏显示)。

## 路由决策矩阵(自上而下,首条匹配)

| 状态 | 键 | 路由 | action |
|---|---|---|---|
| (任意) | Ctrl 或 Alt 组合 | 透传(应用快捷键) | PASSTHROUGH |
| MAGIC | 数字、`+` `-` `=` `[` `]` | member `on_key`(命令文本,如 `#req/news?query=x` 的 `=`) | HANDLED |
| COMPOSING | Space / Enter / Backspace | `sm.step`(提交/强选/删除) | HANDLED(+COMMIT) |
| idle | Space / Enter / Backspace | 透传 | PASSTHROUGH |
| COMPOSING | Escape | reset(取消组合) | HANDLED |
| idle | Escape | 透传 | PASSTHROUGH |
| PANEL_OPEN | Digit 1-9 | `select(idx)` | HANDLED(+COMMIT) |
| 其余 | Digit 1-9 | 透传 | PASSTHROUGH |
| PANEL_OPEN | 方向/Tab/PgUp/PgDn/`[`/`]`/`+`/`=`/`-` | 导航/翻页/移光标 | HANDLED |
| !PANEL_OPEN | 同上 | 透传 | PASSTHROUGH |
| (任意) | Home/End/Delete/Insert、裸修饰键、F1-F12、未识别 keysym | 透传 | PASSTHROUGH |
| (任意) | Char(c) | `sm.step(c)`(idle 内自分流:`/` `#` 进 Snippet、a-z 进 Pinyin、其余透传) | 视 step 结果 |

要点:

- **Digit 0 与其余可打印字符统一走 Char 路径**(历史 quirk:拼音中 `0` 是
  终止符,提交"候选+0")。
- **Escape 门控是 COMPOSING 而非面板开合** —— 组合中但无候选时 Esc 也能
  取消(旧逻辑透传会把 preedit 卡在屏上)。
- `+` `-` `=` `[` `]` 在 `KeyEvent::char` 归一化时就成为导航键:面板展开时
  翻页/移光标,否则透传;MAGIC 模式下是命令文本。这统一了 fcitx 与
  TUI/mock 的行为(旧 TUI 里它们是终止符)。

## 前端契约

**不再拦截任何键。** 把键事件忠实转成 `KeyEvent`:

- fcitx5(C++):组包 `SwiftKeyPacket { sym, unicode, ctrl, shift, alt }`
  → `swift_ime_key()`,按 `ImeView::action` 反应。X keysym → KeyKind 的
  解码表在 Rust 侧(`KeyEvent::from_fcitx`),C++ 不再持有键语义。release
  事件仍是 C++ 丢弃(fcitx5 惯例,事件类型过滤而非内容拦截)。
- TUI(crossterm):`crossterm_to_key()` 映射键类 + 修饰状态。

`swift-ime.h` 中的 `ImeView` 与 `SwiftKeyPacket` 必须与 Rust
`#[repr(C)]` 布局保持一致 —— ABI 变更时 addon 与 cdylib 必须一起重编
(`scripts/build_fcitx.sh`)。
