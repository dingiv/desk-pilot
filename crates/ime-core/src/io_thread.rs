//! IoThread —— 引擎的单条 tokio I/O 线程(事件响应模型)。
//!
//! 预测主路径**不创建线程**;所有异步 I/O(HTTP 请求、剪贴板请求、voice
//! listener)由这一条 tokio 事件循环完成。魔法命令通过
//! [`mpsc::Sender<IoEvent>`](IoEvent) 发事件要求它做事;I/O 完成/变化后经
//! [`FrontEndHandle`](crate::frontend::FrontEndHandle) 推送 `refresh_ui` ——
//! 前端只在收到推送时才拉取视图,不再连续轮询。
//!
//! ## voice listener
//!
//! `SpawnVoiceListener` 事件让事件循环在 IoThread 的 runtime 上 spawn 一个
//! 长生命周期 task,该 task 持有 [`AuraClient`](audio_aura_agent::AuraClient)
//! 与 [`SharedVoiceState`](crate::voice_state::SharedVoiceState),通过
//! `tokio::select!` await SSE 数据面 与 健康探针,把 AsrSegment 折叠进
//! shared state 并触发 `frontend.refresh_ui`。engine drop 时 task 自动 abort,
//! AuraClient 随之 drop —— 整个生命周期是 Arc 引用管理,**无显式 cancel**。

use std::sync::{Arc, Weak};
use std::time::Duration;

use audio_aura_agent::client::AuraClient;
use audio_aura_agent::view::AsrSegment;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::frontend::{BROADCAST_CTX, FrontEndHandle, StateView};
use crate::voice_state::SharedVoiceState;

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
    /// 在 I/O 线程的 main future 内部 spawn 一个 caller 给的 future。绕过
    /// `current_thread` runtime 跨线程 `Handle::spawn` 不 poll remote task 的坑:
    /// 把 spawn intent 推到 channel,main future 收到后调 `tokio::spawn` 直接
    /// 加入本地 ready queue。
    SpawnAux(Box<dyn FnOnce() + Send + 'static>),
    /// 启动 voice listener(引擎构造时一次性发)。listener 拥有 AuraClient,
    /// 通过 SSE 把 AsrSegment 折叠进 `state` 并触发 `frontend.refresh_ui`。
    /// 跟随 IoThread drop 自动 abort。
    SpawnVoiceListener {
        base: String,
        state: Arc<SharedVoiceState>,
        frontend: Weak<dyn FrontEndHandle>,
    },
    /// 停止事件循环。
    Shutdown,
}

/// 引擎 I/O 线程句柄。随引擎创建,引擎 drop 时 runtime drop 自动停止。
#[derive(Clone)]
pub struct IoThread {
    tx: mpsc::Sender<IoEvent>,
    /// 事件循环驻留在这个 runtime 上。字段未被直接读 —— 生命周期管理靠
    /// 它 drop 时停止。
    #[allow(dead_code)]
    rt: Arc<tokio::runtime::Runtime>,
    /// 辅助 task 列表(voice listener 等),让调用方挂自己的长任务,确保随 IoThread
    /// drop 而 abort。空时无开销。`Arc<Mutex<…>>` 是为了让 `IoThread: Clone` 仍成立。
    aux_tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

/// 健康探针间隔(秒)—— listener 在此间隔上检测 aura daemon 连通性。
const HEALTH_PROBE_INTERVAL: Duration = Duration::from_secs(3);

impl IoThread {
    /// 创建并启动 I/O 线程。`frontend` 以 weak 持有,不延长前端生命周期:
    /// 引擎 drop 时,flush 阶段 `upgrade()` 失败 → no-op,不再触达已析构的
    /// C++ 回调。
    pub fn spawn(frontend: Weak<dyn FrontEndHandle>) -> Self {
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .enable_io()
                .build()
                .expect("tokio runtime"),
        );
        let (tx, mut rx) = mpsc::channel::<IoEvent>(64);
        let rt2 = Arc::clone(&rt);
        std::thread::Builder::new()
            .name("ime-io".into())
            .spawn(move || {
                rt2.block_on(async move {
                    // FIXME: 使用 tokio::select! 同时监听 rx 和 其他需要处理的 IO 事件, 而不是使用 tokio::spawn

                    // 事件分发任务。**关键:必须独立 spawn 在 local queue 里,**
                    // 才能被 current_thread runtime 在 main future 让出时 poll 到。
                    // 如果写在 main future 内部 loop,它阻塞在 rx.recv().await 时.
                    // 没人 poll 其他 task(包括 SpawnAux / SpawnVoiceListener)。
                    tokio::spawn(async move {
                        loop {
                            let Some(ev) = rx.recv().await else { break };
                            match ev {
                                IoEvent::Run { ctx, task } => {
                                    task();
                                    if let Some(f) = frontend.upgrade() {
                                        f.refresh_ui(StateView { ctx });
                                    }
                                }
                                IoEvent::RequestClipboard { ctx, count } => {
                                    if let Some(f) = frontend.upgrade() {
                                        f.get_clipboard_item(count);
                                        f.refresh_ui(StateView { ctx });
                                    }
                                }
                                IoEvent::SpawnAux(spawn) => {
                                    spawn();
                                    // 触发 runtime 调度刚 spawn 的 task。
                                    tokio::task::yield_now().await;
                                }
                                IoEvent::SpawnVoiceListener {
                                    base,
                                    state,
                                    frontend,
                                } => {
                                    tokio::spawn(voice_listener_task(base, state, frontend));
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

    /// 发一个事件给 I/O 线程(非阻塞)。线程已死/通道满时静默丢弃。
    pub fn send(&self, ev: IoEvent) {
        let _ = self.tx.try_send(ev);
    }

    /// 启动 voice listener(便捷方法,内部发 `SpawnVoiceListener` 事件)。
    pub fn start_voice_listener(
        &self,
        base: String,
        state: Arc<SharedVoiceState>,
        frontend: Weak<dyn FrontEndHandle>,
    ) {
        self.send(IoEvent::SpawnVoiceListener {
            base,
            state,
            frontend,
        });
    }
}

/// Voice listener task body。在 IoThread 的 current_thread runtime 上运行。
///
/// 通过 `tokio::select!` 同时等待:
/// - SSE 数据面(`AuraClient::subscribe_segments`)
/// - 健康探针(`tokio::time::interval`)
///
/// 二者任一唤醒 → 写 shared state → `frontend.refresh_ui(DEFAULT_CTX)`。
///
/// Task 结束(任一 select 分支失败 / abort):
/// - AuraClient drop,reqwest 连接关闭,SSE stream `next()` 返回 None。
/// - shared state 的 `is_connected()` 保持其最后值(不再变化)。
async fn voice_listener_task(
    base: String,
    state: Arc<SharedVoiceState>,
    frontend: Weak<dyn FrontEndHandle>,
) {
    let client = match AuraClient::new(&base) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, base = %base, "voice listener: AuraClient::new failed");
            return;
        }
    };
    let health_client = client.clone();
    let mut health_tick = tokio::time::interval(HEALTH_PROBE_INTERVAL);
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut segs = Box::pin(client.subscribe_segments());
    loop {
        tokio::select! {
            seg = segs.next() => {
                match seg {
                    Some(seg) => {
                        apply_and_notify(&state, &frontend, seg);
                    }
                    None => {
                        // SSE stream ended (reqwest reconnect loop has internal backoff;
                        // returning None shouldn't normally happen — but if it does we
                        // break and let engine drop clean us up).
                        break;
                    }
                }
            }
            _ = health_tick.tick() => {
                let ok = health_client.health().await.unwrap_or(false);
                state.set_connected(ok);
                tracing::debug!(connected = ok, "health probe → refresh_ui(BROADCAST_CTX)");
                if let Some(f) = frontend.upgrade() {
                    f.refresh_ui(StateView { ctx: BROADCAST_CTX });
                }
            }
        }
    }
    state.set_connected(false);
}

fn apply_and_notify(
    state: &Arc<SharedVoiceState>,
    frontend: &Weak<dyn FrontEndHandle>,
    seg: AsrSegment,
) {
    state.fold_segment(&seg);
    tracing::debug!(
        ?seg,
        "voice segment folded → refresh_ui(BROADCAST_CTX)"
    );
    if let Some(f) = frontend.upgrade() {
        // voice listener 是引擎级全局 SSE —— 不绑定某个输入上下文,用
        // BROADCAST_CTX 让前端刷新所有活动上下文(见 crate::frontend)。
        f.refresh_ui(StateView { ctx: BROADCAST_CTX });
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
    use crate::frontend::FrontEndHandle;

    #[test]
    fn attach_drain_stores_handle() {
        // smoke: spawn_into 不 panic,持有 JoinHandle。
        let front: Arc<dyn FrontEndHandle> = Arc::new(crate::frontend::NoopFrontend::default());
        let io = IoThread::spawn(Arc::downgrade(&front));
        io.send(IoEvent::SpawnAux(Box::new(|| {
            // do nothing — event loop spawned an empty closure and yielded.
        })));
        drop(io);
    }

    /// voice listener 把 SSE 段折叠进 SharedVoiceState。每次 segment 到达
    /// `frontend.refresh_ui` 应被调一次。测试用本地 TCP mock 推送 1 条
    /// stream_fragment 后关闭 —— listener 在 200ms 内应当折叠并刷新。
    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn voice_listener_folds_sse_into_state_and_notifies() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;
        use std::thread;
        use std::time::{Duration, Instant};

        // mock aura SSE server:返回 1 条 stream_fragment 后 hold 连接。
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
                } else if path.starts_with("/api/state") {
                    let body = r#"{"connected":true,"stage3_on":false,"config":{"asr_backend":"","asr_kind":"","asr_provider":"","llm_kind":"","model":"","vad":{"threshold":0.5,"min_silence":0.3,"merge_gap":1.0}},"hotwords":[],"corrections":[]}"#;
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    s.flush().unwrap();
                } else {
                    s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                    s.flush().unwrap();
                }
            }
        });

        struct CountingFrontend(
            StdArc<AtomicUsize>,
            StdArc<std::sync::Mutex<Vec<usize>>>,
        );
        impl FrontEndHandle for CountingFrontend {
            fn get_clipboard_item(&self, _count: u32) {}
            fn refresh_ui(&self, sv: StateView) {
                self.0.fetch_add(1, Ordering::Relaxed);
                self.1.lock().unwrap().push(sv.ctx);
            }
        }
        let refresh_count = StdArc::new(AtomicUsize::new(0));
        let refresh_ctxs = StdArc::new(std::sync::Mutex::new(Vec::new()));
        let front: Arc<dyn FrontEndHandle> =
            Arc::new(CountingFrontend(refresh_count.clone(), refresh_ctxs.clone()));

        let state = Arc::new(SharedVoiceState::new());
        let base = format!("http://127.0.0.1:{port}");
        let io = IoThread::spawn(Arc::downgrade(&front));
        io.start_voice_listener(base.clone(), Arc::clone(&state), Arc::downgrade(&front));

        // 等 listener 折叠 stream_fragment → "你好"
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (_, live) = state.voice_candidates();
            if live == "你好" {
                break;
            }
            if Instant::now() >= deadline {
                panic!("listener 未在 3s 内折叠 SSE 段,live={live:?}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // 至少触发了一次 refresh(可以 > 1 因为 health tick 也调)。
        assert!(
            refresh_count.load(Ordering::Relaxed) >= 1,
            "listener 应至少推一次 refresh"
        );
        // 回归:voice listener 是引擎级全局事件,refresh 必须用 BROADCAST_CTX
        // (0)—— 曾写死 ctx=0 但被 C++ 当作真实输入上下文指针,导致 #asr
        // 候选永远不刷新。前端(含单上下文)据此广播到所有活动上下文。
        let ctxs = refresh_ctxs.lock().unwrap();
        assert!(
            !ctxs.is_empty() && ctxs.iter().all(|c| *c == BROADCAST_CTX),
            "voice refresh 应全部带 BROADCAST_CTX: {ctxs:?}"
        );
    }
}
