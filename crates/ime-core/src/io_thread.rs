//! IoThread —— 引擎的单条 tokio I/O 线程,一个**多事件源 server**。
//!
//! 预测主路径**不创建线程**;所有异步 I/O(HTTP 请求、剪贴板请求、voice)
//! 由这一条 tokio 事件循环完成。主循环用 `tokio::select!` 同时监听:
//! - `rx`(主线程经 [`mpsc::Sender<IoEvent>`](IoEvent) 发来的通用事件 + voice 命令);
//! - `FuturesUnordered`(动态数据源,当前只有 voice 的 aura SSE)。
//!
//! ## 普适的异步能力面(round10)
//!
//! 异步能力是平台能力,不专属于某个魔法命令。任何成员(现有 ASR/REQ/
//! clip,未来的任何命令)经两扇正门使用 I/O 线程:
//! - [`IoThread::spawn_blocking`] —— 阻塞任务 → tokio 阻塞池;
//! - [`IoThread::spawn_task`] —— 异步任务 → 事件循环(async HTTP/SSE/WS)。
//!
//! 两者完成后都统一 `refresh_ui(ctx)`。voice 的长连接会话
//! ([`VoiceSession`])是框架的一个客户端;`#req` 的 async HTTP 是第二个。
//!
//! ## voice 会话(懒惰的)
//!
//! `#asr` 每次预测经 [`VoiceCmd::Attach`] 把 ctx 推给 voice server —— 它此刻才
//! 一次性 `health()` 探针 + 建 SSE 连接;连接后每收到一个 `AsrEvent`,就"顺带"
//! 检查 ctx 是否可用(`refresh_ui` 返回 bool),可用就顺带刷新 UI。失败一次即
//! **放弃**:`active_ctx` 置 -1、丢 SSE 源、不再重连 —— 之后只继续等 `rx`。
//! aura 断联由 `AuraClient` 内部重连兜底,我们不在乎。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::Duration;

use audio_aura_agent::client::AuraClient;
use audio_aura_agent::view::AsrEvent;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio::select;
use tokio::sync::mpsc;

use crate::frontend::{FrontEndHandle, StateView};
use crate::family::magic::SharedVoiceState;

/// 魔法命令发给 I/O 线程的异步工作请求。
///
/// 普适的异步能力面(round10):任何魔法命令(不限于 ASR/REQ)都经
/// [`IoThread::spawn_blocking`] / [`IoThread::spawn_task`] 两扇正门使用
/// I/O 线程 —— 阻塞任务进 tokio 阻塞池,异步任务进事件循环;两者完成
/// 后都会 `refresh_ui(ctx)` 把新候选刷上前端。
pub enum IoEvent {
    /// 阻塞任务(HTTP 拉取等)→ tokio 阻塞池执行,不占事件循环。
    Blocking {
        ctx: usize,
        task: Box<dyn FnOnce() + Send + 'static>,
    },
    /// 异步任务 → 事件循环 runtime 上执行(魔法命令异步 IO 的正门:
    /// async HTTP / SSE / WebSocket …)。
    Task {
        ctx: usize,
        fut: futures::future::BoxFuture<'static, ()>,
    },
    /// 请求前端提供 count 条剪贴板历史(`#clip` 需要时)。
    RequestClipboard { ctx: usize, count: u32 },
    /// voice server 命令(`#asr` 家族发)。复用同一个 rx。
    Voice(VoiceCmd),
    /// 停止事件循环。
    Shutdown,
}

/// magic family → voice server 的命令(主线程发,IoThread 收)。
#[derive(Debug, Clone, Copy)]
pub enum VoiceCmd {
    /// `#asr` 每次预测都发:记录 ctx、若断连则此刻重连、立即刷一次 UI。
    Attach { ctx: usize },
    /// `#asr` 会话退出(deactivate):清掉 ctx。懒惰的 server 也会在下次
    /// `try_refresh` 失败时自行放弃,Detach 只是让它放弃得更早。
    Detach { ctx: usize },
    /// 主动归档(分字符键 `'` = "我说完了"):让 aura 立即关闭当前开放窗口并
    /// 整窗 batch,跳过 merge_gap 剩余等待。未连接时 no-op。
    FlushParagraph,
}

/// 向 voice server 发命令的 typed sender(包装 io thread 的 `tx`)。
#[derive(Clone)]
pub struct VoiceCmdSender(mpsc::Sender<IoEvent>);

impl VoiceCmdSender {
    /// 非阻塞发送;通道满时静默丢弃(命令低频率,64 深足够)。
    pub fn send(&self, cmd: VoiceCmd) {
        let _ = self.0.try_send(IoEvent::Voice(cmd));
    }
}

/// 引擎 I/O 线程句柄。随引擎创建,引擎 drop 时 runtime drop 自动停止。
#[derive(Clone)]
pub struct IoThread {
    tx: mpsc::Sender<IoEvent>,
    /// voice 命令 sender(与 `tx` 同一通道,typed 包装)。
    voice_tx: VoiceCmdSender,
    /// 事件循环驻留在这个 runtime 上。字段未被直接读 —— 生命周期管理靠
    /// 它 drop 时停止。
    #[allow(dead_code)]
    rt: Arc<tokio::runtime::Runtime>,
    /// 辅助 task 列表(voice 等),让调用方挂自己的长任务,确保随 IoThread
    /// drop 而 abort。空时无开销。`Arc<Mutex<…>>` 是为了让 `IoThread: Clone` 仍成立。
    #[allow(dead_code)]
    aux_tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

/// aura SSE 数据流(owned —— 由 `subscribe_events_owned` 产生,可自由移动)。
type Sse = Pin<Box<dyn futures::Stream<Item = AsrEvent>>>;

/// 语音连接空闲超时默认值:退出 `#asr` 后,若长时间没有新的 ASR 使用(Attach),
/// voice server 主动断开 aura,释放连接。可在 `swift-ime.yaml → voice.idle_time`
/// 覆盖(单位:秒)。
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 30;

/// 对 SSE 流的一次 poll:拿到一个段,并**把流原样还回**,让循环决定续传还是放弃。
/// 所有权进出,future 无借用,可放进 `FuturesUnordered`。
async fn poll_event(mut s: Sse) -> Option<(AsrEvent, Sse)> {
    let seg = s.next().await?;
    Some((seg, s))
}

impl IoThread {
    /// 创建并启动 I/O 线程。`voice_base` / `voice_state` 交给 voice server 用;
    /// `idle_timeout` 是语音连接空闲自动断连时长(秒,0 = 永不主动断)。
    /// `frontend` 以 weak 持有,不延长前端生命周期。
    pub fn spawn(
        frontend: Weak<dyn FrontEndHandle>,
        voice_base: String,
        voice_state: Arc<SharedVoiceState>,
        idle_timeout_secs: u64,
    ) -> Self {
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .enable_io()
                .build()
                .expect("tokio runtime"),
        );
        let (tx, rx) = mpsc::channel::<IoEvent>(64);
        let voice_tx = VoiceCmdSender(tx.clone());
        let rt2 = Arc::clone(&rt);
        std::thread::Builder::new()
            .name("ime-io".into())
            .spawn(move || {
                let voice = VoiceSession::new(voice_base, voice_state, idle_timeout_secs);
                rt2.block_on(io_main(rx, frontend, voice));
            })
            .expect("spawn ime io thread");
        IoThread {
            tx,
            voice_tx,
            rt,
            aux_tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// 获取该 I/O 线程 tokio runtime 的 [`Handle`] —— 调用方可以用它在外部
    /// runtime 上 `spawn` 自己的 task,使其驻留于 IoThread 的事件循环。
    /// **handle 仅在 IoThread 存活期间有效**(drop 后调用方 task 会 abort)。
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    /// 发一个事件给 I/O 线程(非阻塞)。线程已死/通道满时静默丢弃。
    pub fn send(&self, ev: IoEvent) {
        let _ = self.tx.try_send(ev);
    }

    /// **普适异步能力面 1/2**:阻塞任务 → tokio 阻塞池执行(不占事件
    /// 循环),完成后 `refresh_ui(ctx)`。同步 HTTP 拉取等。
    pub fn spawn_blocking(&self, ctx: usize, task: impl FnOnce() + Send + 'static) {
        self.send(IoEvent::Blocking {
            ctx,
            task: Box::new(task),
        });
    }

    /// **普适异步能力面 2/2**:异步任务 → 事件循环 runtime 执行,完成后
    /// `refresh_ui(ctx)`。async HTTP / SSE / WebSocket —— 魔法命令的
    /// 异步 IO 正门(不占阻塞线程,不阻塞事件循环)。
    pub fn spawn_task(&self, ctx: usize, fut: impl Future<Output = ()> + Send + 'static) {
        self.send(IoEvent::Task {
            ctx,
            fut: Box::pin(fut),
        });
    }

    /// voice 命令 sender(与 `tx` 同一通道)—— `#asr` 家族经它发 Attach/Detach。
    pub fn voice_tx(&self) -> VoiceCmdSender {
        self.voice_tx.clone()
    }
}

/// IoThread 主循环:多事件源 server。
///
/// SSE 事件源(FuturesUnordered 元素类型)。
type SseSource = Pin<Box<dyn Future<Output = Option<(AsrEvent, Sse)>>>>;

/// ASR 语音会话 —— 通用 I/O 循环的一个**客户端**(round10 分层:框架
/// 不含 voice 语义,voice 只是第一个把长连接挂上事件循环的会话;req 是
/// 第二个,走 [`IoThread::spawn_task`])。
///
/// 状态:活跃 ctx 跟踪、连接标志、空闲断连;SSE 源集(`sources`)由
/// 循环持有,方法经参数借用(与 `select!` 的臂借用互不重叠)。
struct VoiceSession {
    base: String,
    state: Arc<SharedVoiceState>,
    /// 当前有活 `#asr` 会话的 ctx;-1 = 无。
    active_ctx: i64,
    /// 是否持有 aura SSE 源(连接中)。
    connected: bool,
    /// 最近一次 ASR 使用(Attach)时刻 —— 空闲超时据此断开 aura。
    last_activity: tokio::time::Instant,
    /// 空闲自动断连时长;`None` = 永不主动断(配置 `voice.idle_time: 0`)。
    idle_timeout: Option<Duration>,
}

impl VoiceSession {
    fn new(base: String, state: Arc<SharedVoiceState>, idle_timeout_secs: u64) -> Self {
        VoiceSession {
            base,
            state,
            active_ctx: -1,
            connected: false,
            last_activity: tokio::time::Instant::now(),
            idle_timeout: (idle_timeout_secs > 0).then(|| Duration::from_secs(idle_timeout_secs)),
        }
    }

    /// `#asr` Attach:记录活跃 ctx、断连则重连(health 探针 → 历史全量
    /// 同步 → 建流)。返回 `is_new`(新 ctx 才立即刷一次 —— 同 ctx 的重复
    /// Attach 来自刷新驱动的重预测,是 no-op,否则 refresh → magic_tick →
    /// predict → Attach → refresh 会乒乓循环)。
    async fn attach(
        &mut self,
        ctx: usize,
        frontend: &Weak<dyn FrontEndHandle>,
        sources: &mut FuturesUnordered<SseSource>,
    ) -> bool {
        let is_new = self.active_ctx != ctx as i64;
        self.active_ctx = ctx as i64;
        self.last_activity = tokio::time::Instant::now();
        tracing::info!(ctx, is_new, connected = self.connected, "voice Attach");
        if !self.connected && !self.state.is_mock() {
            // 断裂/未连 → 触发重连:一次性 health 探针种 is_connected
            // (UI 显示"未连接"还是"语音识别中"),然后**总是**建流 ——
            // 断连由流内部指数退避重试,连续失败超上限流结束(→ None →
            // connected=false),下次 #asr Attach 重建流 = 手动重连。
            match AuraClient::new(&self.base) {
                Ok(client) => {
                    let ok = client.health().await.unwrap_or(false);
                    self.state.set_conn(if ok {
                        crate::family::magic::voice_state::VoiceConn::Connected
                    } else {
                        crate::family::magic::voice_state::VoiceConn::Failed
                    });
                    tracing::info!(connected = ok, "voice attach → spawn stream");
                    // **重连全量同步**:断连期间 aura 可能已定稿若干句
                    // (数据面 SSE append-only,新订阅收不到历史)。
                    // 先清空本地历史(避免旧句残留),再拉一次 `/api/results`
                    // 灌入最近的定稿 —— 这样 `#asr` 重新打开时首个候选
                    // 是断连期间说的那句话,而不是旧残留。
                    if ok {
                        self.state.reset();
                        match client.results().await {
                            Ok(history) => {
                                self.state.sync_history(&history);
                                tracing::info!(
                                    count = history.len(),
                                    "voice reconnect → synced aura history"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "voice reconnect: results sync failed");
                            }
                        }
                    }
                    // SSE 流连接状态回调:连不上/连上都要**及时**汇报
                    // UI(用户修的 bug —— 之前流内部退避重连时,状态
                    // 一直停在旧值,`#asr` 显示旧语音)。广播通知所有
                    // 打开 #asr 的上下文,让它们重新 predict 拉最新状态。
                    let st = Arc::clone(&self.state);
                    let fe = frontend.clone();
                    let on_conn = Box::new(move |c: audio_aura_agent::client::SseConnState| {
                        // 连接状态变化 → 及时汇报 UI。Connected → 🎙;
                        // 其它(Failed / 退避中的 Connecting)都视为
                        // 连不上 → 「语音服务暂不可用」,避免闪烁。
                        // (首次"正在连接"由 Attach 分支的 health 探针
                        // 负责;流内部的退避重试一律按不可用处理。)
                        use audio_aura_agent::client::SseConnState as S;
                        let v = if c == S::Connected {
                            crate::family::magic::voice_state::VoiceConn::Connected
                        } else {
                            crate::family::magic::voice_state::VoiceConn::Failed
                        };
                        st.set_conn(v);
                        notify_conn(&fe, -1); // 广播:有 #asr 的 ctx 刷新
                    });
                    sources.push(Box::pin(poll_event(Box::pin(
                        client.subscribe_events_owned_with_conn(Some(on_conn)),
                    ))));
                    self.connected = true;
                }
                Err(e) => {
                    tracing::error!(error = %e, base = %self.base, "voice: AuraClient::new failed");
                    self.state.set_conn(crate::family::magic::voice_state::VoiceConn::Failed);
                    // 连接失败 → 及时汇报前端(否则 UI 停在"正在连接")。
                    notify_conn(frontend, self.active_ctx);
                }
            }
        }
        is_new
    }

    /// `#asr` 会话退出(deactivate):清掉 ctx。懒惰的循环也会在下次
    /// `try_refresh` 失败时自行放弃,Detach 只是让它放弃得更早。
    fn detach(&mut self, ctx: usize) {
        if self.active_ctx == ctx as i64 {
            self.active_ctx = -1;
        }
    }

    /// 主动归档(分字符键 `'` = "我说完了"):让 aura 立即关闭当前开放窗口并
    /// 整窗 batch,跳过 merge_gap 剩余等待。未连接时 no-op。fire-and-forget
    /// —— 归档结果经 SSE 数据面回流。
    async fn flush_paragraph(&mut self) {
        if self.connected && !self.state.is_mock() {
            self.last_activity = tokio::time::Instant::now();
            match AuraClient::new(&self.base) {
                Ok(client) => {
                    if let Err(e) = client.flush_paragraph().await {
                        tracing::warn!(error = %e, "flush_paragraph failed");
                    } else {
                        tracing::info!("flush_paragraph → aura(主动归档)");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "flush_paragraph: client"),
            }
        }
    }

    /// SSE 段折叠 + 连通性汇报 + 活动心跳。有活跃 `#asr` 会话 → 顺带刷新
    /// UI;无会话(后台监听)→ 只折叠不刷新。
    fn on_sse_event(&mut self, ev: &AsrEvent, frontend: &Weak<dyn FrontEndHandle>) {
        self.state.fold_event(ev);
        // 收到句事件 = 已连上(即使 Attach 时 health 误判为断,段也证活)。
        self.state
            .set_conn(crate::family::magic::voice_state::VoiceConn::Connected);
        // 后台语音也算活动 —— 空闲超时只在"无 #asr 且无语音"时断开。
        self.last_activity = tokio::time::Instant::now();
        tracing::info!(?ev, "voice event folded");
        if self.active_ctx >= 0 {
            try_refresh(&mut self.active_ctx, frontend);
        }
    }

    /// SSE 流结束 → 丢源,不再 select。流断 = 语音服务暂不可用(源由循环
    /// 丢弃;下次 `#asr` Attach 触发重连)。
    fn on_stream_ended(&mut self, frontend: &Weak<dyn FrontEndHandle>) {
        tracing::warn!("voice SSE stream ended → 丢源,不重连");
        self.connected = false;
        self.state
            .set_conn(crate::family::magic::voice_state::VoiceConn::Failed);
        // 流断 → 及时汇报前端,UI 从 🎙 切到「语音服务暂不可用」。
        notify_conn(frontend, self.active_ctx);
    }

    /// 空闲超时 select 臂的 guard:配置了超时、无活跃 `#asr` 会话、仍持连接。
    fn idle_guard(&self) -> bool {
        self.idle_timeout.is_some() && self.active_ctx < 0 && self.connected
    }

    /// 空闲超时的 deadline(`idle_timeout = None` 时给一个极远的哨兵值,
    /// 配合 guard 永不触发)。
    fn idle_deadline(&self) -> tokio::time::Instant {
        self.idle_timeout
            .map(|t| self.last_activity + t)
            .unwrap_or(tokio::time::Instant::now() + Duration::from_secs(86400 * 365))
    }

    /// 空闲断连:drop SSE 源关闭连接;断开 = 语音服务暂不可用,下次
    /// `#asr` 输入时 Attach 触发重连。
    fn idle_disconnect(
        &mut self,
        frontend: &Weak<dyn FrontEndHandle>,
        sources: &mut FuturesUnordered<SseSource>,
    ) {
        tracing::info!("voice 空闲超过 {:?} → 主动断开 aura", self.idle_timeout);
        sources.clear(); // drop SSE 源 → reqwest 连接关闭。
        self.connected = false;
        self.state
            .set_conn(crate::family::magic::voice_state::VoiceConn::Failed);
        // 空闲断连发生在后台(active_ctx < 0)→ 广播,让打开的 #asr
        // 上下文从 🎙 切到「语音服务暂不可用」。
        notify_conn(frontend, self.active_ctx);
    }
}

/// 通用 I/O 事件循环(原 `voice_server_main` —— round10 分层:循环只认
/// [`IoEvent`] 与会话回调,voice 语义全在 [`VoiceSession`])。
async fn io_main(
    mut rx: mpsc::Receiver<IoEvent>,
    frontend: Weak<dyn FrontEndHandle>,
    mut voice: VoiceSession,
) {
    let mut sources: FuturesUnordered<SseSource> = FuturesUnordered::new();

    loop {
        select! {
            // 臂 1:主线程发来的事件(含 voice 命令)。
            ev = rx.recv() => {
                match ev {
                    Some(IoEvent::Voice(VoiceCmd::Attach { ctx })) => {
                        let is_new = voice.attach(ctx, &frontend, &mut sources).await;
                        if is_new {
                            // 立即刷一次(即使未连上,predict 也会显示"未连接")。
                            try_refresh(&mut voice.active_ctx, &frontend);
                        }
                    }
                    Some(IoEvent::Voice(VoiceCmd::Detach { ctx })) => {
                        voice.detach(ctx);
                    }
                    Some(IoEvent::Voice(VoiceCmd::FlushParagraph)) => {
                        voice.flush_paragraph().await;
                    }
                    Some(IoEvent::Blocking { ctx, task }) => {
                        // 阻塞任务(HTTP 拉取等)放 tokio 阻塞池执行:既不让 current_thread
                        // 事件循环被卡住(否则语音段会停摆),也避免在 async 上下文里做阻塞
                        // 调用(后者在 runtime drop 时会 panic "Cannot drop a runtime in a
                        // context where blocking is not allowed")。
                        if tokio::task::spawn_blocking(task).await.is_err() {
                            tracing::warn!("io blocking task panicked");
                        }
                        if let Some(f) = frontend.upgrade() {
                            f.refresh_ui(StateView { ctx });
                        }
                    }
                    Some(IoEvent::RequestClipboard { ctx, count }) => {
                        if let Some(f) = frontend.upgrade() {
                            f.get_clipboard_item(count);
                            f.refresh_ui(StateView { ctx });
                        }
                    }
                    Some(IoEvent::Task { ctx, fut }) => {
                        // 异步任务 spawn 到事件循环(脱离 select —— 长任务
                        // 不阻塞其他事件);完成后 refresh,把命令的新候选
                        // 刷上前端(与 Blocking 臂对称)。
                        let frontend = frontend.clone();
                        tokio::spawn(async move {
                            fut.await;
                            if let Some(f) = frontend.upgrade() {
                                f.refresh_ui(StateView { ctx });
                            }
                        });
                    }
                    Some(IoEvent::Shutdown) | None => break,
                }
            }
            // 臂 2:动态数据源 —— aura SSE 段。空源时禁用(零轮询)。
            ev = sources.next(), if !sources.is_empty() => {
                match ev {
                    Some(Some((ev, s))) => {
                        voice.on_sse_event(&ev, &frontend);
                        // **始终续传源**:后台监听也要保持连接、持续折叠数据 ——
                        // 不因"无会话/刷新失败"丢源(否则后台识别只收 1~3 字就断)。
                        sources.push(Box::pin(poll_event(s)));
                    }
                    Some(None) => voice.on_stream_ended(&frontend),
                    None => {} // sources 空(带 guard 不应发生)
                }
            }
            // 臂 3:空闲超时 —— 无活跃 #asr 会话、仍持连接,且超过空闲时长没有
            // 新 Attach → 主动断开 aura,释放连接(长连接不常驻)。
            _ = tokio::time::sleep_until(voice.idle_deadline()),
                if voice.idle_guard() =>
            {
                voice.idle_disconnect(&frontend, &mut sources);
            }
        }
    }
}

/// **状态变化时向前端及时汇报**(有风吹草动就要通知,不等用户按键)。
///
/// - 有活跃 `#asr` 会话(`active_ctx >= 0`)→ 定向刷新该 ctx;
/// - 无活跃会话 → 广播 `BROADCAST_CTX`,让所有打开了 `#asr` 的上下文刷新
///   (断连/连接失败发生在后台监听时,UI 也要从旧状态切到「语音服务暂不可用」)。
///
/// 与 [`try_refresh`] 不同:这里**只通知、不裁决续传** —— 断连事件不改
/// `active_ctx`,`sources` 由调用方自己决定。
fn notify_conn(frontend: &Weak<dyn FrontEndHandle>, active_ctx: i64) {
    let ctx = if active_ctx >= 0 {
        active_ctx as usize
    } else {
        crate::frontend::BROADCAST_CTX
    };
    tracing::info!("notify_conn");
    if let Some(f) = frontend.upgrade() {
        f.refresh_ui(StateView { ctx });
        tracing::info!("notify_conn 2");
    }
}

/// 顺带刷新 UI,并裁决"是否继续"。
///
/// 返回 true = ctx 有效、刷新已触发,源应续传;false = 放弃(`active_ctx` 置 -1)。
fn try_refresh(active_ctx: &mut i64, frontend: &Weak<dyn FrontEndHandle>) -> bool {
    if *active_ctx < 0 {
        tracing::debug!(active_ctx = *active_ctx, "voice try_refresh: 无 ctx,跳过");
        return false;
    }
    if let Some(f) = frontend.upgrade() {
        let ok = f.refresh_ui(StateView { ctx: *active_ctx as usize });
        tracing::info!(active_ctx = *active_ctx, ok, "voice try_refresh");
        if ok {
            return true;
        }
    } else {
        tracing::debug!(active_ctx = *active_ctx, "voice try_refresh: 前端已销毁");
    }
    *active_ctx = -1;
    tracing::info!("voice try_refresh → 放弃(active_ctx=-1)");
    false
}

impl Drop for IoThread {
    fn drop(&mut self) {
        // Abort 任何挂着的辅助 task。runtime drop 会做同样的事,但显式 abort
        // 让 task 在自己的 drop glue 跑前先收到 cancel 信号,更可预测。
        let mut tasks = self.aux_tasks.lock().unwrap();
        for h in tasks.drain(..) {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::FrontEndHandle;

    #[test]
    fn attach_drain_stores_handle() {
        // smoke: spawn 不 panic。
        let front: Arc<dyn FrontEndHandle> = Arc::new(crate::frontend::NoopFrontend::default());
        let state = Arc::new(SharedVoiceState::new());
        let io = IoThread::spawn(
            Arc::downgrade(&front),
            "http://127.0.0.1:1".into(),
            state,
            DEFAULT_IDLE_TIMEOUT_SECS,
        );
        io.spawn_task(
            0,
            async {
                // do nothing — event loop spawned an empty future and refreshed.
            },
        );
        drop(io);
    }

    /// `try_refresh` 语义:ctx 有效且前端接受 → true 续传;前端拒绝 → 自愈置 -1。
    #[test]
    fn try_refresh_gives_up_when_frontend_rejects() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        struct Reject(StdArc<AtomicUsize>);
        impl FrontEndHandle for Reject {
            fn get_clipboard_item(&self, _count: u32) {}
            fn refresh_ui(&self, _: StateView) -> bool {
                self.0.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
        let calls = StdArc::new(AtomicUsize::new(0));
        let front: Arc<dyn FrontEndHandle> = Arc::new(Reject(calls.clone()));

        let mut ctx = 0xCAFEi64;
        assert!(!try_refresh(&mut ctx, &Arc::downgrade(&front)), "reject → give up");
        assert_eq!(ctx, -1, "active_ctx 自愈置 -1");
        // ctx 已 -1 → 直接放弃,不再触达前端。
        assert!(!try_refresh(&mut ctx, &Arc::downgrade(&front)));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "放弃后不再 refresh");
    }

    #[test]
    fn try_refresh_accepts_when_ctx_valid() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        struct Accept(StdArc<AtomicUsize>);
        impl FrontEndHandle for Accept {
            fn get_clipboard_item(&self, _count: u32) {}
            fn refresh_ui(&self, sv: StateView) -> bool {
                self.0.store(sv.ctx, Ordering::Relaxed);
                true
            }
        }
        let seen = StdArc::new(AtomicUsize::new(0));
        let front: Arc<dyn FrontEndHandle> = Arc::new(Accept(seen.clone()));

        let mut ctx = 0xBEEFi64;
        assert!(try_refresh(&mut ctx, &Arc::downgrade(&front)));
        assert_eq!(ctx, 0xBEEFi64, "有效 ctx 不变");
        assert_eq!(seen.load(Ordering::Relaxed), 0xBEEF, "refresh 带上 ctx");
    }

    /// 集成:Attach{ctx} → voice server 连 aura(mock SSE)→ 折叠段 → refresh_ui
    /// 收到**该 ctx**。这是 #asr 候选刷新的核心路径。
    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn attach_connects_and_refreshes_target_ctx() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;
        use std::thread;
        use std::time::{Duration, Instant};

        // mock aura:GET /health → 200;GET /api/asr_stream → 推 1 条 stream_fragment。
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { break };
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut req = [0u8; 4096];
                let _ = s.read(&mut req);
                let path = std::str::from_utf8(&req)
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                if path.starts_with("/api/asr_stream") {
                    let hdr = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                    s.write_all(hdr).unwrap();
                    s.write_all(b"data: {\"type\":\"stream_fragment\",\"window_id\":1,\"segment_id\":1,\"text\":\"\\u4f60\\u597d\",\"at_s\":0}\n\n").unwrap();
                    s.flush().unwrap();
                    thread::sleep(Duration::from_secs(1));
                } else {
                    // /health 等一律 200。
                    s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .unwrap();
                    s.flush().unwrap();
                }
            }
        });

        struct CountingFrontend(StdArc<AtomicUsize>, StdArc<std::sync::Mutex<Vec<usize>>>);
        impl FrontEndHandle for CountingFrontend {
            fn get_clipboard_item(&self, _count: u32) {}
            fn refresh_ui(&self, sv: StateView) -> bool {
                self.0.fetch_add(1, Ordering::Relaxed);
                self.1.lock().unwrap().push(sv.ctx);
                true
            }
        }
        let refresh_count = StdArc::new(AtomicUsize::new(0));
        let refresh_ctxs = StdArc::new(std::sync::Mutex::new(Vec::new()));
        let front: Arc<dyn FrontEndHandle> = Arc::new(CountingFrontend(
            refresh_count.clone(),
            refresh_ctxs.clone(),
        ));
        let state = Arc::new(SharedVoiceState::new());
        let base = format!("http://127.0.0.1:{port}");
        let io = IoThread::spawn(
            Arc::downgrade(&front),
            base,
            Arc::clone(&state),
            DEFAULT_IDLE_TIMEOUT_SECS,
        );

        // #asr 家族发 Attach{ctx} —— 指向真实输入上下文指针,不是广播。
        io.send(IoEvent::Voice(VoiceCmd::Attach { ctx: 0xCAFE }));

        // 等 voice server 连上并折叠 stream_fragment → "你好"。
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (_, live) = state.voice_candidates();
            if live == "你好" {
                break;
            }
            if Instant::now() >= deadline {
                panic!("voice server 未在 3s 内折叠 SSE 段,live={live:?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // refresh 至少一次:必须有一次**定向**到 Attach 的 ctx(段折叠触发),
        // 且允许连接状态回调的**广播**(BROADCAST_CTX=0)混入 —— 那是
        // "有风吹草动就汇报"的预期行为。
        assert!(
            refresh_count.load(Ordering::Relaxed) >= 1,
            "voice server 应至少推一次 refresh"
        );
        let ctxs = refresh_ctxs.lock().unwrap();
        assert!(
            ctxs.contains(&0xCAFE),
            "至少一次 refresh 定向到 Attach 的 ctx: {ctxs:?}"
        );
    }
}