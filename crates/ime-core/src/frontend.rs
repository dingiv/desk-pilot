//! FrontEndHandle — 引擎 I/O 线程与前端之间的推送回调。
//!
//! 前端不再轮询:`#asr` / `#req` / `#clip` 等魔法命令把异步工作发给引擎的
//! 单条 tokio I/O 线程([`crate::io_thread`]),I/O 完成/变化后经此句柄**推送**
//! 给前端 —— 前端只在收到推送时才拉取最新视图并渲染。

/// 前端句柄。由前端(fcitx5 / TUI / mock)实现,引擎构造函数注入。
pub trait FrontEndHandle: Send + Sync {
    /// 请求前端提供 `count` 条剪贴板历史(`#clip` 需要时)。前端取到后经
    /// `set_clipboard_history` 回填引擎,再触发一次 `refresh_ui`。
    fn get_clipboard_item(&self, count: u32);

    /// 通知前端:上下文 `state_view.ctx` 的异步状态推进了(voice 流式 / req
    /// 结果落地 / 剪贴板回填),请重渲染。这是**轻量信号** —— I/O 线程不碰
    /// 状态机,前端收到后在主循环调 [`crate::engine::ImeEngine::get_live_view`]
    /// 拉取最新视图再渲染。
    ///
    /// `ctx` 约定:事件绑定某个输入上下文时传该上下文的 ctx 值;引擎级全局
    /// 事件(voice listener 的 SSE 段 / 健康探针)不绑定任何上下文,传
    /// [`BROADCAST_CTX`] —— 前端应刷新它**所有**活动上下文。
    fn refresh_ui(&self, state_view: StateView);
}

/// 一次 UI 推送信号:哪个输入上下文的异步状态变了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateView {
    pub ctx: usize,
}

/// 广播哨兵:引擎级异步推进不绑定某个输入上下文,`refresh_ui` 用
/// `ctx = 0` 表示"刷新所有活动上下文"。
///
/// 契约(跨 FFI,fcitx5 的 C++ `onRefresh` 依赖它):
/// - fcitx5 前端:C++ 侧对 `ctx == 0` 遍历 `activeContexts_` 逐出一次
///   `swift_ime_magic_tick` —— 只有处于 live 魔法会话(`#asr`)的 context
///   产生新视图,其余 `magic_tick_ctx` 返回 `None` 天然跳过;
/// - 单上下文前端(TUI / mock):默认 ctx=0 即唯一上下文,等价于广播。
pub const BROADCAST_CTX: usize = 0;

/// 空前端句柄(测试 / 无前端场景):记录 refresh 信号,剪贴板请求不响应。
#[derive(Debug, Default)]
pub struct NoopFrontend {
    /// 收到的 refresh 信号(fctx)记录,测试断言用。
    pub refreshes: std::sync::Mutex<Vec<usize>>,
}

impl FrontEndHandle for NoopFrontend {
    fn get_clipboard_item(&self, _count: u32) {}
    fn refresh_ui(&self, state_view: StateView) {
        self.refreshes.lock().unwrap().push(state_view.ctx);
    }
}
