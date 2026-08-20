//! IoThread —— 引擎的单条 tokio I/O 线程(事件响应模型)。
//!
//! 预测主路径**不创建线程**;所有异步 I/O(HTTP 请求、voice 订阅、剪贴板
//! 请求)由这一条 tokio 事件循环完成。魔法命令通过
//! [`mpsc::Sender<IoEvent>`](IoEvent) 发事件要求它做事;I/O 完成/变化后经
//! [`FrontEndHandle`](crate::frontend::FrontEndHandle) 推送 `refresh_ui` ——
//! 前端只在收到推送时才拉取视图,不再连续轮询。

use std::sync::{Arc, Weak};

use tokio::sync::mpsc;

use crate::frontend::StateView;

/// 魔法命令发给 I/O 线程的异步工作请求。
pub enum IoEvent {
    /// 在 I/O 线程上跑一个同步任务(可能阻塞,如 HTTP 拉取)。任务完成后
    /// `refresh_ui(ctx)` 让前端重渲染。成员把"写异步状态"的闭包发过来,
    /// 事件循环不阻塞预测主路径。
    Run {
        ctx: usize,
        task: Box<dyn FnOnce() + Send + 'static>,
    },
    /// 请求前端提供 count 条剪贴板历史(`#clip` 需要时)。
    RequestClipboard { ctx: usize, count: u32 },
    /// 订阅某上下文对共享 AsrBuffer 版本变化的监听 —— 变化即 `refresh_ui(ctx)`。
    /// `buffer` 是该上下文的语音缓冲(I/O 线程持有的 watcher 读它检测变化)。
    /// `last` 是订阅时刻的 `buffer.version()` —— **由 caller 在 send 前取值**,
    /// 而不是 IoThread 收到事件时再读(避免 race:bridge 在 ensure_subscribed
    /// send 与 IoThread 处理之间 bump version,导致 watcher 错过第一次变化)。
    VoiceSubscribe {
        ctx: usize,
        buffer: Arc<crate::asr_buffer::AsrBuffer>,
        last: u64,
    },
    /// 取消订阅(命令退出时,watcher 停止对该 ctx 的轮询)。
    VoiceUnsubscribe { ctx: usize },
    /// 在 I/O 线程的 main future 内部 spawn 一个 caller 给的 future。绕过
    /// `current_thread` runtime 跨线程 `Handle::spawn` 不 poll remote task 的坑:
    /// 把 spawn intent 推到 channel,main future 收到后调 `tokio::spawn` 直接
    /// 加入本地 ready queue。
    SpawnAux(Box<dyn FnOnce() + Send + 'static>),
    /// 停止事件循环。
    Shutdown,
}

/// 语音订阅表:ctx → (AsrBuffer, 上次版本)。watcher 任务读它,事件循环改它。
type VoiceSubscriptions = std::sync::Mutex<std::collections::HashMap<usize, (Arc<crate::asr_buffer::AsrBuffer>, u64)>>;

/// 引擎 I/O 线程句柄。随引擎创建,引擎 drop 时 runtime drop 自动停止。
#[derive(Clone)]
pub struct IoThread {
    tx: mpsc::Sender<IoEvent>,
    /// 事件循环驻留在这个 runtime 上。字段未被直接读 —— 生命周期管理靠
    /// 它 drop 时停止。
    #[allow(dead_code)]
    rt: Arc<tokio::runtime::Runtime>,
    /// 辅助 task 列表(aura drain 等),让调用方挂自己的长任务,确保随 IoThread drop
    /// 一并 abort。空时无开销。`Arc<Mutex<…>>` 是为了让 `IoThread: Clone` 仍成立。
    aux_tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl IoThread {
    /// 创建并启动 I/O 线程。`frontend` 以 weak 持有,不延长前端生命周期:
    /// 引擎 drop 时,flush 阶段 `upgrade()` 失败 → no-op,不再触达已析构的
    /// C++ 回调。
    pub fn spawn(frontend: Weak<dyn crate::frontend::FrontEndHandle>) -> Self {
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                // aura HTTP(SSE)与未来可能的 input I/O 都依赖 tokio 的 TCP。
                // current_thread runtime 默认不开启 IO,这里显式 enable。
                .enable_io()
                .build()
                .expect("tokio runtime"),
        );
        let (tx, mut rx) = mpsc::channel::<IoEvent>(64);
        let rt2 = Arc::clone(&rt);
        let watcher_front = Arc::new(frontend.clone());
        let front = frontend;
        // 语音订阅表:ctx → (AsrBuffer, 上次版本)。watcher 任务读它,事件循环改它。
        let subscribed: Arc<VoiceSubscriptions> = Arc::new(VoiceSubscriptions::default());
        std::thread::Builder::new()
            .name("ime-io".into())
            .spawn(move || {
                rt2.block_on(async move {
                    // 语音 watcher:每 50ms 检查订阅的 AsrBuffer 版本,变化即
                    // 推送 refresh_ui(ctx) —— 仅在有订阅时工作(按需)。
                    let watcher_sub = Arc::clone(&subscribed);
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            let mut changed: Vec<usize> = Vec::new();
                            {
                                let mut sub = watcher_sub.lock().unwrap();
                                for (ctx, (buf, last)) in sub.iter_mut() {
                                    let v = buf.version();
                                    if v != *last {
                                        *last = v;
                                        changed.push(*ctx);
                                    }
                                }
                            }
                            for ctx in changed {
                                if let Some(f) = watcher_front.upgrade() {
                                    f.refresh_ui(StateView { ctx });
                                }
                            }
                        }
                    });

                    // 事件分发任务。**关键:必须独立 spawn 在 local queue 里,**
                    // 才能被 current_thread runtime 在 main future 让出时 poll 到。
                    // 如果写在 main future 内部 loop,它阻塞在 rx.recv().await 时
                    // 没人 poll 其他 task(包括 SpawnAux 推过来的 spawn intent)。
                    tokio::spawn(async move {
                        loop {
                            let Some(ev) = rx.recv().await else { break };
                            match ev {
                                IoEvent::Run { ctx, task } => {
                                    task();
                                    if let Some(f) = front.upgrade() {
                                        f.refresh_ui(StateView { ctx });
                                    }
                                }
                                IoEvent::RequestClipboard { ctx, count } => {
                                    if let Some(f) = front.upgrade() {
                                        f.get_clipboard_item(count);
                                        f.refresh_ui(StateView { ctx });
                                    }
                                }
                                IoEvent::VoiceSubscribe { ctx, buffer, last } => {
                                    // last 由 caller 在 send 前取的 buffer.version() —— 避开
                                    // send → 处理 之间 buffer 被 bump 的 race。
                                    subscribed.lock().unwrap().insert(ctx, (buffer, last));
                                    tracing::debug!(ctx, "voice subscribed");
                                }
                                IoEvent::VoiceUnsubscribe { ctx } => {
                                    subscribed.lock().unwrap().remove(&ctx);
                                    tracing::debug!(ctx, "voice unsubscribed");
                                }
                                IoEvent::SpawnAux(spawn) => {
                                    spawn();
                                    // 触发 runtime 调度刚 spawn 的 task。
                                    tokio::task::yield_now().await;
                                }
                                IoEvent::Shutdown => break,
                            }
                        }
                    });

                    // Main future 仅做"等待 shutdown" —— 它不阻塞 channel,
                    // 因为 channel 已由上面的独立 task 消费。Main future 自己
                    // 永远不 Ready,block_on 永不返回,线程持续运行。
                    std::future::pending::<()>().await;
                });
            })
            .expect("spawn ime io thread");
        IoThread {
            tx,
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

    /// 把一个 task 挂到 IoThread 的辅助列表 —— 跟随 IoThread 析构而 abort,
    /// 防止调用方的 task 引用已被 drop 的数据。
    pub fn attach_drain(&self, h: tokio::task::JoinHandle<()>) {
        self.aux_tasks.lock().unwrap().push(h);
    }

    /// 把"在 main future 内调 `tokio::spawn`"的意图推到 IoThread,绕开
    /// `current_thread` runtime 跨线程 `Handle::spawn` 在 main future 阻塞时不
    /// poll remote task 的问题。
    ///
    /// `factory` 在 main future 内被调(那时已在 runtime 上下文),负责
    /// `tokio::spawn(future)` 并返回 JoinHandle。返回的 handle 会自动 push 到
    /// aux_tasks,随 IoThread drop 而 abort。
    pub fn spawn_into<F>(&self, factory: F)
    where
        F: FnOnce() -> tokio::task::JoinHandle<()> + Send + 'static,
    {
        let aux = Arc::clone(&self.aux_tasks);
        let factory: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
            let h = factory();
            aux.lock().unwrap().push(h);
        });
        let _ = self.tx.try_send(IoEvent::SpawnAux(factory));
    }

    /// 发一个事件给 I/O 线程(非阻塞)。线程已死/通道满时静默丢弃。
    pub fn send(&self, ev: IoEvent) {
        let _ = self.tx.try_send(ev);
    }
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
    use crate::asr_buffer::AsrBuffer;
    use crate::ImeView;

    /// 验证 watcher 链路:engine 注册 voice 订阅 + AsrBuffer::set_live 触发
    /// 版本 bump → IoThread watcher 检测 → `FrontEndHandle::refresh_ui` 被调。
    ///
    /// 这是 fcitx 模式下"#asr 后说话候选项实时更新"的等价路径 —— 用一个
    /// `RecordingFrontend` 收集 refresh 回调,断言 watcher 推了至少两次
    /// refresh(对应两次 set_live)。
    #[test]
    fn voice_watcher_pushes_refresh_to_frontend() {
        use crate::frontend::FrontEndHandle;
        struct RecordingFrontend {
            refreshes: std::sync::Mutex<Vec<usize>>,
        }
        impl FrontEndHandle for RecordingFrontend {
            fn get_clipboard_item(&self, _count: u32) {}
            fn refresh_ui(&self, sv: StateView) {
                self.refreshes.lock().unwrap().push(sv.ctx);
            }
        }
        let front_rec = Arc::new(RecordingFrontend {
            refreshes: std::sync::Mutex::new(Vec::new()),
        });
        let front: Arc<dyn FrontEndHandle> = front_rec.clone();
        let io = IoThread::spawn(Arc::downgrade(&front));

        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true);
        // 模拟"#asr 进入" 时的订阅:VoiceSubscribe 走 IoThread::send,
        // 被独立事件循环 task 消费 → subscribed.insert(ctx, (buf, last))。
        io.send(IoEvent::VoiceSubscribe { ctx: 0, buffer: Arc::clone(&buf), last: buf.version() });
        // 给事件循环机会处理(独立 task 收到 → yield_now → runtime 排 watcher)。
        std::thread::sleep(std::time::Duration::from_millis(100));

        // 第一次 set_live → 版本 0 → 1 → watcher 50ms 内 push refresh。
        buf.set_live("你好");
        std::thread::sleep(std::time::Duration::from_millis(150));
        let refreshes = front_rec.refreshes.lock().unwrap().len();
        assert!(refreshes >= 1, "watcher 应该至少 push 一次 refresh,实际 {refreshes}");

        // 第二次 live:再 push 一次。
        let prev = refreshes;
        buf.set_live("你好世界");
        std::thread::sleep(std::time::Duration::from_millis(150));
        let now = front_rec.refreshes.lock().unwrap().len();
        assert!(now > prev, "第二次 live变化应再 push 一次 refresh:{prev} → {now}");

        // 取消订阅,不应再 push。
        io.send(IoEvent::VoiceUnsubscribe { ctx: 0 });
        std::thread::sleep(std::time::Duration::from_millis(100));
        let before_idle = front_rec.refreshes.lock().unwrap().len();
        buf.set_live("无声");
        std::thread::sleep(std::time::Duration::from_millis(150));
        let after_idle = front_rec.refreshes.lock().unwrap().len();
        assert_eq!(before_idle, after_idle, "unsubscribe 后不应再 push: {before_idle} → {after_idle}");
    }

    /// 完整链路:watcher 检测 voice 变化 → refresh_ui → 测试桩调 magic_tick_ctx →
    /// engine 重建候选。模拟 fcitx 主循环的行为。
    #[test]
    fn voice_change_drives_magic_tick_rebuild() {
        use crate::engine::ImeEngine;
        use crate::frontend::FrontEndHandle;
        use crate::router::KeyEvent;

        let (refresh_tx, refresh_rx) = std::sync::mpsc::channel::<usize>();
        struct Trampoline(std::sync::mpsc::Sender<usize>);
        impl FrontEndHandle for Trampoline {
            fn get_clipboard_item(&self, _count: u32) {}
            fn refresh_ui(&self, sv: StateView) {
                let _ = self.0.send(sv.ctx);
            }
        }
        let front: Arc<dyn FrontEndHandle> = Arc::new(Trampoline(refresh_tx));
        // 构造带 Trampoline frontend 的 engine —— engine 内部 IoThread 会用这个
        // frontend 推送 refresh。
        let engine = ImeEngine::with_config(
            crate::family::pinyin::PinyinWeights::default(),
            crate::family::english::EnglishWeights::default(),
            Box::new(crate::expander::DefaultProvider),
            Vec::new(),
            crate::scoring::ScoringConfig::default(),
            Arc::clone(&front),
        );
        let buf = Arc::new(AsrBuffer::new());
        buf.set_connected(true);
        engine.set_asr_buffer(Arc::clone(&buf));

        const CTX: usize = 0;
        // 模拟 fcitx 调 swift_ime_key(ic=CTX, '#') + asr 输入
        for c in "#asr".chars() {
            let _ = engine.predict_ctx(CTX, c);
        }

        // 模拟 aura bridge set_live 一次 —— watcher 应 push refresh。
        buf.set_live("你好");
        // 等 watcher 50ms + 缓冲。
        std::thread::sleep(std::time::Duration::from_millis(150));
        let ctx_seen = refresh_rx.recv_timeout(std::time::Duration::from_millis(500))
            .expect("watcher 应 push refresh");
        assert_eq!(ctx_seen, CTX);

        // 模拟 fcitx 主循环调 magic_tick:候选应反映新 live。
        let view = engine.magic_tick_ctx(CTX).expect("tick 重建");
        let live_in_view = (0..view.candidate_count as usize).any(|i| {
            ImeView::str_field(&view.candidates[i].text).contains("你好")
        });
        assert!(live_in_view, "新 live '你好' 应进候选: candidate_count={}", view.candidate_count);

        // 再 set_live 一次 —— 重复流程。
        buf.set_live("你好世界");
        std::thread::sleep(std::time::Duration::from_millis(150));
        let ctx_seen = refresh_rx.recv_timeout(std::time::Duration::from_millis(500))
            .expect("再次 watcher push");
        assert_eq!(ctx_seen, CTX);
        let view = engine.magic_tick_ctx(CTX).expect("二次 tick");
        let live_in_view = (0..view.candidate_count as usize).any(|i| {
            ImeView::str_field(&view.candidates[i].text).contains("你好世界")
        });
        assert!(live_in_view, "二次 live '你好世界' 应进候选: candidate_count={}", view.candidate_count);
    }

    #[test]
    fn attach_drain_stores_handle() {
        // smoke: spawn_into 不 panic,持有 JoinHandle。
        let front: Arc<dyn crate::frontend::FrontEndHandle> =
            Arc::new(crate::frontend::NoopFrontend::default());
        let io = IoThread::spawn(Arc::downgrade(&front));
        io.spawn_into(|| tokio::spawn(async {}));
        drop(io);
    }
}
