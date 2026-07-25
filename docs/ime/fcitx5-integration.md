# swift-ime × fcitx5 集成技术文档

> 基于 fcitx5 5.1.14 + fcitx5-chinese-addons 源码分析。2026-07-23。

## 1. fcitx5 架构概览

### 1.1 输入法引擎 = Addon

fcitx5 的输入法引擎是一个 **addon**(`.so` 共享库),由 fcitx5 进程 `dlopen` 加载。引擎要做的三件事:

1. **注册工厂**(`FCITX_ADDON_FACTORY` 宏) → fcitx5 通过工厂创建引擎实例
2. **接收按键**(`keyEvent()`) → 把按键转换成候选词/提交文本
3. **管理每窗口状态**(`InputContextProperty`) → 每个应用窗口有独立状态

### 1.2 核心类

```
InputMethodEngine              ← 引擎基类
  ├─ keyEvent()                ← ★ 唯一纯虚函数,必须实现
  ├─ activate()                ← 切换到本引擎时调用(默认空)
  ├─ deactivate()              ← 切走时调用(默认调 reset)
  ├─ reset()                   ← 焦点变化/Esc 时调(默认空)
  └─ filterKey()               ← 拦截未处理按键(可选)

InputContext                    ← 一个应用窗口的输入上下文
  ├─ keyEvent()                ← fcitx5 框架→引擎的入口
  ├─ focusIn() / focusOut()    ← 焦点变化
  ├─ destroy()                 ← 窗口关闭(无对应引擎回调!)
  ├─ commitString()            ← 引擎提交文本到应用
  ├─ updatePreedit()           ← 通知应用更新预编辑文本
  ├─ updateUserInterface()     ← 刷新候选窗/状态区
  └─ propertyFor(factory)      ← ★ 获取 per-InputContext 状态

InputContextProperty           ← per-InputContext 状态的基类
  └─ 随 InputContext::destroy() 自动销毁

InputContextPropertyFactory    ← 工厂,注册到 InputContextManager
  FactoryFor<T>                ← 便捷模板 (= LambdaInputContextPropertyFactory<T>)
```

## 2. per-InputContext 状态管理

### 2.1 官方模式: InputContextProperty

fcitx5-pinyin 的做法(文件: `im/pinyin/pinyin.h`):

```cpp
// 1. 定义 per-context 状态类
class PinyinState : public InputContextProperty {
    PinyinState(PinyinEngine *engine);
    libime::PinyinContext context_;  // 拼音输入状态
    // ...
};

// 2. 在引擎类里声明工厂
class PinyinEngine : public InputMethodEngineV3 {
    FactoryFor<PinyinState> factory_;  // 工厂
};

// 3. 构造函数里注册
PinyinEngine::PinyinEngine(Instance *instance) {
    instance->inputContextManager().registerProperty("pinyinState", &factory_);
}

// 4. keyEvent 里获取当前窗口的状态
void PinyinEngine::keyEvent(...) {
    auto *ic = event.inputContext();
    auto *state = ic->propertyFor(&factory_);  // ★ 取 per-window 状态
    // ...
}
```

**生命周期**: `PinyinState` 随 `InputContext` 创建(`factory_.create(ic)`),随 `InputContext::destroy()` 销毁。引擎不需要手动清理。

### 2.2 我们的模式: HashMap<InputContext*, StateMachine>

因为我们走 C ABI(Rust ↔ C++ 跨语言),不能直接用 `InputContextProperty`,所以用 `HashMap<usize, StateMachine>` 按 `InputContext*` 指针隔离:

```rust
// ffi.rs
static CONTEXTS: LazyLock<Mutex<HashMap<usize, StateMachine>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_ctx(ctx: *const c_void, f: impl FnOnce(&Dispatcher, &mut StateMachine) -> T) -> T {
    let sm = CONTEXTS.lock().unwrap()
        .entry(ctx as usize).or_default();  // 首次按键自动创建
    f(DISPATCHER.get().unwrap(), sm)
}
```

**C++ 侧传入 `ctx`**:
```cpp
void SwiftImeEngine::keyEvent(...) {
    auto *ic = keyEvent.inputContext();
    int action = swift_ime_process_key((void *)ic, ch, out_text, ...);
}
```

**清理机制**: `deactivate` 时 `remove` 条目。

### 2.3 两种模式对比

| | InputContextProperty | HashMap<*, StateMachine> |
|---|---|---|
| 创建 | `factory_.create(ic)` | `.entry(ctx).or_default()` 首次按键懒创建 |
| 销毁 | 随 InputContext 自动 | `deactivate` 回调 remove |
| 跨语言 | C++ 内部 | C ABI `void *ctx` 透传 |
| 内存安全 | 框架保证 | 手动管理(依赖 deactivate 覆盖所有销毁路径) |

### 2.4 deactivate 的调用时机

**关键事实**: fcitx5 在以下两种情况调 `deactivate`:
1. 用户切换到另一个输入法(Super+Space)
2. 使用本引擎的窗口关闭(`InputContext::destroy` 前触发)

所以 `deactivate → remove` 能覆盖所有需要清理的场景——这是正确的。

## 3. 引擎生命周期精确时序

```
fcitx5 启动
  └─ dlopen("swift-ime.so")
       └─ FCITX_ADDON_FACTORY → SwiftImeFactory::create()
            └─ swift_ime_init(nullptr)          ← ★ 全局初始化(仅一次)

用户打开应用窗口
  └─ InputContext 创建
       └─ InputContextProperty 创建(PinyinState 等)

用户切换输入法到 swift-ime
  └─ engine.activate(entry, event)
       └─ swift_ime_activate(ctx)

用户键入 'n'
  └─ InputContext::keyEvent(KeyEvent)
       └─ engine.keyEvent(entry, event)
            └─ swift_ime_process_key(ctx, 'n', ...)
                 └─ with_ctx → dispatcher.process_key → sm.step
                      → ImeAction::Candidates { ... }
            └─ ic->inputPanel().setCandidateList(...)
            └─ ic->updateUserInterface(InputPanel)

用户按 Space 选词
  └─ 候选窗调用 SwiftCandidateWord::select(ic)
       └─ swift_ime_select_candidate(ctx, idx, ...)
            └─ sm.select(idx) → ImeAction::Commit(text)
       └─ ic->commitString(text)

用户按 Esc 或焦点切换
  └─ engine.reset(entry, event)
       └─ swift_ime_reset(ctx)
            └─ sm.reset()

用户切换走输入法
  └─ engine.deactivate(entry, event)
       └─ swift_ime_deactivate(ctx)
            └─ CONTEXTS.lock().remove(ctx)      ← ★ 清理

窗口关闭
  └─ InputContext::destroy()
       └─ InputContextDestroyedEvent
       └─ engine.deactivate(entry, event)       ← ★ 调了 deactivate
            └─ CONTEXTS.lock().remove(ctx)      ← ★ 所以能清理
       └─ InputContextProperty 自动销毁
```

## 4. keyEvent → 候选/提交 完整链路

以 fcitx5-pinyin 为参考,对照我们的实现:

| 步骤 | fcitx5-pinyin (C++) | swift-ime (Rust C ABI) |
|---|---|---|
| 1. 取每窗口状态 | `auto *state = ic->propertyFor(&factory_)` | `with_ctx(ctx, \|disp, sm\| { ... })` |
| 2. 按键→字符 | `Key::keySymToUnicode(sym)` | 同,在 C++ engine.cpp 完成 |
| 3. 按键消费 | `event.filterAndAccept()` | 同,在 C++ engine.cpp |
| 4. 业务逻辑 | `state->context_.type(ch)` → `PinyinContext` 处理 | `sm.step(ch, env)` → `StateMachine` FSM |
| 5. 取候选 | `context.candidatesToCursor()` | `state.sm.candidates.clone()`(缓存) |
| 6. 填候选窗 | `CommonCandidateList + setPageSize + append` | 同模式,在 C++ engine.cpp |
| 7. 显示 preedit | `ic->inputPanel().setClientPreedit(text)` | 同,out_text 带拼音 buffer |
| 8. 提交文本 | `ic->commitString(sentence)` | 同,action==Commit 分支 |
| 9. 候选选词 | `CandidateWord::select(ic)` → `commitString` | `SwiftCandidateWord::select` → `commitString`（同模式） |

**核心差异**:
- fcitx5-pinyin 把拼音引擎(`libime::PinyinIME`)和上下文(`PinyinContext`)放在 C++ 进程内,通过 `InputContextProperty` 管理生命周期
- 我们把引擎(`Dispatcher`)放在 Rust 侧,上下文(`StateMachine`)通过 `HashMap<InputContext*, SM>` 管理,生命周期依赖 C ABI 回调

### 2.5 deactivate 区分 FocusOut vs 切换输入法

fcitx5-pinyin 的 `deactivate` 通过 `event.type()` 区分两种场景:

```cpp
void PinyinEngine::deactivate(const InputMethodEntry &entry,
                               InputContextEvent &event) {
    if (event.type() != EventType::InputContextSwitchInputMethod) {
        // FocusOut: 仅 reset,不 commit(窗口失焦不应产生文本)
        reset(entry, event);
        return;
    }
    // IM-switch: commit pending preedit, then reset
    auto *ic = event.inputContext();
    auto *state = ic->propertyFor(&factory_);
    if (!state->context_.userInput().empty()) {
        ic->commitString(state->context_.preedit());
        state->context_.clear();
    }
    reset(entry, event);
}
```

当前我们的 `deactivate` 直接 `remove`(不区分场景)。Phase 2 可改进:FocusOut 时 commit buffer + remove,IM-switch 时仅 remove。

## 5. 我们当前实现的正确性评估

### ✅ 正确的设计

1. **HashMap<InputContext*, SM> 隔离每窗口**:等价于 `InputContextProperty` 模式,指针唯一性保证不串状态
2. **deactivate → remove 清理**:deactivate 覆盖所有销毁路径(IME 切换 + 窗口关闭),**不会泄漏**
3. **首次按键懒创建**:`.entry().or_default()` 自动为每个新 InputContext 创建 StateMachine
4. **C ABI 透传 ctx**:`(void *)inputContext()` 作为首参,语义等价于 `propertyFor`
5. **Dispatcher 全局单例**:`OnceLock<Dispatcher>` 共享引擎(仅初始化一次,和 fcitx5-pinyin `PinyinEngine` 单例一致)

### ⚠️ 与官方模式的差异(非错误,需注意)

| 差异 | 影响 | 建议 |
|---|---|---|
| 懒创建 vs 属性工厂预创建 | 引擎构造函数不创建状态,需等到首次按键 | 可接受。如需预创建,在 `activate` 回调强制 `.entry().or_default()` |
| `reset` 不清 HashMap,只清 SM 内部 | reset 后状态清空,条目保留——正常,reset 不意味着窗口关闭 | OK |
| 无 `copyTo` 支持(多焦点共享状态) | fcitx5 罕见场景(多 seat),我们忽略 | OK |
| `deactivate` 直接 `remove` vs fcitx5-pinyin 先 `commit` | 用户切走再切回来会丢失正在打的拼音 | Phase 2 可优化:`deactivate` 时先 commit buffer 再 remove |

## 6. 候选窗翻页: 用 CommonCandidateList 内置分页

fcitx5-pinyin 的翻页配置:

```cpp
// 构造候选窗时
candidateList->setPageSize(*config_.pageSize);           // 默认 7
candidateList->setCursorPositionAfterPaging(
    CursorPositionAfterPaging::ResetToFirst);             // 翻页后光标归首

// keyEvent 中翻页键处理
auto *pageable = candidateList->toPageable();
if (pageable->hasPrev()) { pageable->prev(); }
if (pageable->hasNext()) { pageable->next(); }
```

`CommonCandidateList` 内置 `PageableCandidateList` 接口,翻页键由配置控制(默认 `-`/`=`)。我们的引擎只需在 `action==3` 分支加 `setPageSize(7)`,翻页由 fcitx5 框架处理:

```cpp
case 3: {
    auto list = std::make_unique<fcitx::CommonCandidateList>();
    list->setPageSize(7);                    // ★ 加这行
    list->setCursorPositionAfterPaging(      // ★ 加这行
        CursorPositionAfterPaging::ResetToFirst);
    // ... append candidates ...
    ic->inputPanel().setCandidateList(std::move(list));
    ic->updateUserInterface(InputPanel);
}
```

## 7. ENTER = commitRawInput 的官方实现

fcitx5-pinyin 的 Enter 处理验证了我们的设计:

```cpp
// 配置项: commitRawInput 键列表(默认: Return, KP_Enter)
if (event.key().checkKeyList(*config_.commitRawInput)) {
    ic->commitString(preeditCommitString(ic));  // 提交 preedit 原文
    state->context_.clear();
    event.filterAndAccept();
}
```

我们的 `StateMachine::pinyin_enter()` → `Commit(raw_buffer)` 语义和官方完全一致。

## 8. 关键参考文件

| 文件 | 内容 |
|---|---|
| `fcitx5-chinese-addons/im/pinyin/pinyin.h` | PinyinEngine 类定义 + PinyinState + 配置 |
| `fcitx5-chinese-addons/im/pinyin/pinyin.cpp` | keyEvent 完整流程 + 候选窗构建 + Enter 处理 |
| `fcitx5/src/lib/fcitx/inputmethodengine.h` | InputMethodEngine 接口(activate/deactivate/reset/keyEvent) |
| `fcitx5/src/lib/fcitx/inputcontext.h` | InputContext 接口(含 propertyFor/destroy/commitString) |
| `fcitx5/src/lib/fcitx/inputcontextproperty.h` | InputContextProperty + FactoryFor 模板 |
| `fcitx5/src/lib/fcitx/candidatelist.h` | CommonCandidateList + PageableCandidateList |
| `fcitx5/src/lib/fcitx/inputpanel.h` | InputPanel(setClientPreedit/setCandidateList) |

## 9. 候选窗 UI 控制能力

fcitx5 的候选窗由 **引擎**(填数据) 和 **UI addon**(画像素) 分离协作。引擎通过 `InputPanel` 控制显示什么,UI addon(classicui/kimpanel)负责渲染。

### 9.1 InputPanel 布局

```
┌─── AuxUp ───────────────────┐   setAuxUp("nihao")       ← 候选上方的拼音
│                             │
│  1.你好  2.泥壕  3.尼...     │   setCandidateList(...)    ← 候选列表
│                             │
└─── AuxDown ─────────────────┘   setAuxDown("提示")        ← 候选下方(罕见)
```

### 9.2 CommonCandidateList 内置交互

| 能力 | API | 说明 |
|---|---|---|
| 分页 | `setPageSize(7)` | 每页 7 个,`-`/`=` 翻页 |
| 翻页后光标归位 | `setCursorPositionAfterPaging(ResetToFirst)` | 翻页后高亮第一候选 |
| 数字键选词 | `setSelectionKey(keys)` | 默认 1-9,不设自动绑定 |
| 候选间移动 | `toCursorMovable()->nextCandidate()` | 上下箭头移动高亮 |
| 自定义标签 | `setLabels({"①","②",...})` | 替换数字标签 |
| 布局方向 | `setLayoutHint(Vertical/Horizontal)` | 横排或竖排 |
| 右键菜单 | `setActionableImpl(...)` | 每候选可加操作(fcitx5 ≥ 5.1.10) |

### 9.3 引擎控制 vs 需要自写 UI

| 能力 | 框架提供 | 需要自写 UI addon |
|---|---|---|
| 候选内容(文字、顺序) | ✅ 引擎完全控制 | — |
| 拼音显示(AuxUp) | ✅ `setAuxUp` | — |
| 数字键选词 | ✅ 内置(1-9) | — |
| 翻页(`-`/`=` PgUp/PgDn) | ✅ 内置(`setPageSize`) | — |
| 方向键移动候选光标 | ✅ 内置 | — |
| 候选颜色/字体/大小 | ❌ | 自写 `UserInterface` addon |
| 候选窗位置/形状 | ❌ | 自写 UI addon |
| 候选上图标/emoji/富文本 | ❌ | 自写 UI addon |
| snippet 展开预览 | ❌ | 自写 UI addon 或走 familiar socket |
| #asr 语音缓冲状态 | ❌ | 同上 |
| 候选右键菜单 | ⚠️ `ActionableCandidateList`(5.1.10) | 框架原生,需 ≥5.1.10 |

### 9.4 自写 UI addon 框架

如需自定义外观,可写独立 `UserInterface` addon(`.so`):

```cpp
class MyUI : public UserInterface {
    void update(UserInterfaceComponent c, InputContext *ic) override {
        // 拿到 InputPanel 全部数据(auxUp, auxDown, preedit, candidateList)
        // 用任意工具渲染: GTK4, egui, Skia, QML...
    }
    bool available() override;
    void suspend() override;
    void resume() override;
};
```

参考实现:
- fcitx5 classicui: ~4000 行 C++/XCB 渲染
- fcitx5 kimpanel: KDE 面板风格,DBus 通信

**策略**:Phase 1-2 用框架内置 UI(CommonCandidateList 已覆盖中文输入基础体验)。Phase 3 如需自定义外观/emoji/语音状态,写 Rust + GTK4 UI addon(和 familiar 同栈)。
