//! IoThread —— 引擎的单条 tokio I/O 线程(事件响应模型)。
//!
//! 预测主路径**不创建线程**;所有异步 I/O(HTTP 请求、voice 订阅、剪贴板
//! 请求)由这一条 tokio 事件循环完成。魔法命令通过
//! [`mpsc::Sender<IoEvent>`](IoEvent) 发事件要求它做事;I/O 完成/变化后经
//! [`FrontEndHandle`](crate::frontend::FrontEndHandle) 推送 `refresh_ui` ——
//! 前端只在收到推送时才拉取视图,不再连续轮询。

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::frontend::{FrontEndHandle, StateView};

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
    VoiceSubscribe { ctx: usize, buffer: Arc<crate::asr_buffer::AsrBuffer> },
    /// 取消订阅(命令退出时,watcher 停止对该 ctx 的轮询)。
    VoiceUnsubscribe { ctx: usize },
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
}

impl IoThread {
    /// 创建并启动 I/O 线程。`frontend` 供 I/O 完成时推送刷新。
    pub fn spawn(frontend: Arc<dyn FrontEndHandle>) -> Self {
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("tokio runtime"),
        );
        let (tx, mut rx) = mpsc::channel::<IoEvent>(64);
        let rt2 = Arc::clone(&rt);
        let front = Arc::clone(&frontend);
        // 语音订阅表:ctx → (AsrBuffer, 上次版本)。watcher 任务读它,事件循环改它。
        let subscribed: Arc<VoiceSubscriptions> = Arc::new(VoiceSubscriptions::default());
        std::thread::Builder::new()
            .name("ime-io".into())
            .spawn(move || {
                rt2.block_on(async move {
                    // 语音 watcher:每 50ms 检查订阅的 AsrBuffer 版本,变化即
                    // 推送 refresh_ui(ctx) —— 仅在有订阅时工作(按需)。
                    let watcher_front = Arc::clone(&frontend);
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
                                watcher_front.refresh_ui(StateView { ctx });
                            }
                        }
                    });

                    // 事件循环:分发 IoEvent。
                    loop {
                        let Some(ev) = rx.recv().await else { break };
                        match ev {
                            IoEvent::Run { ctx, task } => {
                                // 同步任务(可能阻塞 HTTP)在 I/O 线程跑,完成后
                                // 推送前端刷新对应上下文。
                                task();
                                front.refresh_ui(StateView { ctx });
                            }
                            IoEvent::RequestClipboard { ctx, count } => {
                                front.get_clipboard_item(count);
                                front.refresh_ui(StateView { ctx });
                            }
                            IoEvent::VoiceSubscribe { ctx, buffer } => {
                                let last = buffer.version();
                                subscribed.lock().unwrap().insert(ctx, (buffer, last));
                                tracing::debug!(ctx, "voice subscribed");
                            }
                            IoEvent::VoiceUnsubscribe { ctx } => {
                                subscribed.lock().unwrap().remove(&ctx);
                                tracing::debug!(ctx, "voice unsubscribed");
                            }
                            IoEvent::Shutdown => break,
                        }
                    }
                });
            })
            .expect("spawn ime io thread");
        IoThread { tx, rt }
    }

    /// 发一个事件给 I/O 线程(非阻塞)。线程已死/通道满时静默丢弃。
    pub fn send(&self, ev: IoEvent) {
        let _ = self.tx.try_send(ev);
    }
}
