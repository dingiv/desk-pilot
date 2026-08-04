# aura API v2 迁移设计

## 变更对比

| 功能 | 旧 API (v1) | 新 API (v2) |
|---|---|---|
| ASR 事件 | `GET /api/stream` → SSE: hello/interim/final/status | `GET /api/stream?state_changed_frequency=N` → SSE ping, 然后 `GET /api/state` 取快照 |
| 状态 | 分散在 SSE 事件中 | 单一 `AuraStateView` 快照 |
| Scout 控制 | `POST /api/control/scout {enabled}` | 不变 |
| 纠偏 | `POST /api/correct {seq, raw, corrected}` | 不变 |
| 音频 | `GET /api/audio/{seq}` | 不变 |
| 健康检查 | `GET /health` | 不变 |

## 新数据模型 AuraStateView

```rust
AuraStateView {
    connected: bool,                      // scout 是否活跃
    stage3_on: bool,                      // Stage3 agent 是否开启
    config: ConfigView { asr_backend, asr_kind, asr_provider, llm_kind, model, vad },
    hotwords: Vec<String>,
    corrections: Vec<CorrectionView>,     // {raw, corrected}
    utterances: Vec<UtteranceView>,
}

UtteranceView {
    seq: u64,
    partial: String,                      // 实时流式结果
    calibrated: Option<String>,           // Stage2 临时校准
    final_: Option<FinalView>,            // 最终结果（句子结束时填充）
    live: bool,                           // 仍在识别中
    corrected_by_user: bool,
    at_s: f64,
}

FinalView {
    raw: String,
    streaming: String,
    calibrated: String,                   // 权威校准文本
    intent: String,
    reply: String,
    route_ms: f64,
}
```

## geek-familiar 改造计划

### 1. 依赖变更

```toml
# apps/geek-familiar/Cargo.toml 新增
audio-aura-agent = { path = "../../crates/aura-agent" }
tokio = { version = "1", features = ["rt-multi-thread"] }
futures = "0.3"
```

移除旧依赖：不再需要 `serde_json` 的手动 SSE 解析（已由 SDK 处理）。

### 2. 替换 asr.rs → 直接使用 AuraClient

**旧代码**：`service/asr.rs` — 手动 TcpStream 解析 SSE 事件 → `AsrUpdate` 枚举  
**新方案**：`tokio::spawn` + `AuraClient::subscribe(400)` → 直接产 `AuraStateView` 快照  

```rust
// iced subscription 中：
Subscription::run_with(base_url, |base| {
    Box::pin(iced::stream::channel(16, move |mut tx| async move {
        // spawn tokio task in dedicated thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let client = AuraClient::new(&base).unwrap();
                // 立即取一次
                if let Ok(snap) = client.state().await {
                    let _ = tx.try_send(Message::AuraState(snap));
                }
                let mut stream = Box::pin(client.subscribe(400));
                while let Some(snap) = stream.next().await {
                    if tx.try_send(Message::AuraState(snap)).is_err() { break; }
                }
            });
        });
        std::future::pending::<()>().await;
    }))
})
```

### 3. 替换 aura.rs HTTP → AuraClient 方法

| 旧函数 | 新方法 |
|---|---|
| `aura::health(addr)` | `client.health().await` |
| `aura::control_scout(addr, enabled)` | `client.set_connected(enabled).await` |
| `aura::correct(addr, seq, raw, corrected)` | `client.correct(seq, raw, corrected).await` |
| `aura::fetch_audio(addr, seq)` | `client.audio(seq).await` |

这些在 `Message` handler 中的 `Task::perform` 里调用，需要在 tokio runtime 中执行。

### 4. 替换数据模型

| 旧 | 新 |
|---|---|
| `AsrUpdate` 枚举 | `AuraStateView` 快照 |
| `AsrState { connected, interim, history }` | 直接从 `AuraStateView.utterances` 提取 |
| `ConversationTurn { seq, user_text, intent, reply }` | 使用 `UtteranceView` + `FinalView` |

### 5. 消息流

**旧流程**：
```
asr.rs: spawn thread → TcpStream → parse SSE → callback → AsrUpdate → iced channel
```

**新流程**：
```
AuraClient::subscribe(400) → Stream<AuraStateView> → iced channel → Message::AuraState
```

State 更新逻辑在 `update()` 中处理：
```rust
Message::AuraState(view) => {
    self.asr.sse_connected = true;  // SSE is up
    self.asr.scout_active = view.connected;
    
    // Update transcription history + transcript buffer
    for u in &view.utterances {
        if let Some(f) = &u.final_ {
            // This is a finalized utterance — update or insert
            self.upsert_utterance(f, u.seq, u.at_s);
        }
        // Live utterance — update interim partial
        if u.live {
            self.asr.interim = u.calibrated.clone().unwrap_or_else(|| u.partial.clone());
        }
    }
    
    // Rebuild transcript from all final utterances
    self.rebuild_transcript();
    
    // Update hotwords, corrections
    self.hotwords = view.hotwords.clone();
    self.corrections = view.corrections.iter().map(|c| (c.raw.clone(), c.corrected.clone())).collect();
}
```

### 6. 文件变更清单

| 文件 | 操作 |
|---|---|
| `Cargo.toml` | 添加 `audio-aura-agent`, `tokio`, `futures` |
| `src/service/asr.rs` | **删除**（由 SDK 替代） |
| `src/service/aura.rs` | **删除**（由 SDK 替代） |
| `src/service/mod.rs` | 移除两个子模块 |
| `src/model/mod.rs` | 简化 `AsrState`，移除 `AsrUpdate`, `ConversationTurn` |
| `src/app.rs` | 重写 subscription 和 update handler，使用 `AuraStateView` |
| `src/view/chat.rs` | transcript 内容由 `AuraStateView.utterances` 驱动 |

### 7. 注意事项

- **tokio runtime**：iced 内部用 smol，reqwest 需要 tokio。在独立线程中 `Runtime::new()` + `block_on` 隔离，不阻塞 iced UI 线程。
- **线程安全**：`iced::futures::channel::mpsc::Sender::try_send()` 是线程安全的，可以在 tokio 线程中调用。
- **向后兼容**：老 aura daemon 仍支持 `/api/stream`（无 `state_changed_frequency` 参数），但 snapshot API 是新接口。迁移后需要新 daemon。
- **state_changed throttling**：daemon 保证 ≥250ms 间隔，以 `subscribe(freq_ms)` 参数为准。
