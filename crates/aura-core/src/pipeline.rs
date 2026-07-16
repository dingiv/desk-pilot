//! pipeline.rs — the hot loop (Stage1 store + Stage2 merged 整流+路由 via RouterEngine) and the
//! writer fallback (remote Anthropic-compatible LLM, non-streaming). Emits StreamEvent JSON identical
//! to web/types.ts so the devtools-web is unchanged. Port of src/service.ts's handleTurn/runWriter.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{AppState, LlmConfig};

const RECENT_CONTEXT: i64 = 6;

const WRITER_SYSTEM: &str = "你是一位专业中文写作 worker。你会收到：(1) 一段写作任务简述；(2) 用户通过语音口述、已整流成书面文本的一组素材片段（按时间顺序）。\
把这些零散、可能前后跳跃甚至自我修正的口述素材，重排逻辑、去重压缩、润色，产出一篇结构清晰的文章/文档。\
要求：用 Markdown，以一级标题（# ）开头，合理分小节；忠于素材原意不编造具体数字；以最新意图为准前后统一；直接输出正文，不要开场白。";

#[derive(Serialize)]
pub struct Secretary {
    pub intent: String,
    pub reply: String,
    pub task_id: Option<String>,
}

#[derive(Serialize)]
pub struct TurnResult {
    pub chunk_id: String,
    pub node_id: String,
    pub calibrated_text: String,
    pub merged: bool,
    pub topic_id: String,
    pub secretary: Secretary,
}

pub struct TurnInput {
    pub raw_text: String,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub topic_id: Option<String>,
    pub audio_base64: Option<String>,
    pub audio_mime: Option<String>,
    pub duration_ms: Option<i64>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ensure_topic(state: &AppState, topic_id: Option<&str>) -> Result<String> {
    if let Some(t) = topic_id {
        if state.store.get_topic(t)?.is_some() {
            return Ok(t.to_string());
        }
    }
    if let Some(t) = state.store.list_topics()?.first() {
        return Ok(t.topic_id.clone());
    }
    let id = Uuid::new_v4().to_string();
    state.store.create_topic(&id, "默认话题")?;
    Ok(id)
}

pub async fn handle_turn(state: Arc<AppState>, input: TurnInput) -> Result<TurnResult> {
    let topic_id = ensure_topic(&state, input.topic_id.as_deref())?;
    let now = now_ms();
    let chunk_id = Uuid::new_v4().to_string();

    // ── Stage 1: audio (best-effort) + raw chunk ──
    let mut audio_path: Option<String> = None;
    let mut audio_mime: Option<String> = None;
    if let (Some(b64), Some(mime)) = (input.audio_base64.as_ref(), input.audio_mime.as_ref()) {
        if let Ok(p) = crate::audio::save_audio(&state.audio_dir, &chunk_id, b64, mime).await {
            audio_path = Some(p);
            audio_mime = Some(mime.clone());
        }
    }
    state.store.create_chunk(&audio_aura_store::NewChunk {
        chunk_id: &chunk_id,
        start_time: input.start_time.unwrap_or(now),
        end_time: input.end_time.unwrap_or(now),
        raw_text: &input.raw_text,
        audio_path: audio_path.as_deref(),
        audio_mime: audio_mime.as_deref(),
        duration_ms: input.duration_ms,
        topic_id: Some(&topic_id),
    })?;
    state.emit(json!({"type":"chunk","chunk_id":chunk_id,"raw_text":input.raw_text,"topic_id":topic_id,"has_audio":audio_path.is_some()}));

    // ── Stage 2: merged 整流+路由 via local RouterEngine (off the async threads) ──
    let context: Vec<String> = state
        .store
        .get_recent_nodes(RECENT_CONTEXT, Some(&topic_id))?
        .into_iter()
        .rev()
        .map(|n| n.calibrated_text)
        .collect();
    let ctx_str = if context.is_empty() { None } else { Some(context.join("\n")) };
    let router = state.router.clone();
    let raw = input.raw_text.clone();
    let raw_out =
        tokio::task::spawn_blocking(move || router.route_blocking(&raw, ctx_str.as_deref(), &[])).await??;
    let decision = audio_aura_router::parse_decision(&raw_out, &input.raw_text);

    // node (M-transport: each turn its own node, no cross-turn merge yet)
    let node_id = Uuid::new_v4().to_string();
    state
        .store
        .create_node(&node_id, &[chunk_id.clone()], &decision.calibrated_text, Some(&topic_id))?;
    state.store.mark_chunk_calibrated(&chunk_id)?;
    state.emit(json!({"type":"node","node_id":node_id,"calibrated_text":decision.calibrated_text,"linked_chunks":[chunk_id],"merged":false,"topic_id":topic_id}));

    // ── Secretary ──
    let mut task_id: Option<String> = None;
    if decision.intent == "task" {
        if let Some(t) = &decision.task {
            let id = Uuid::new_v4().to_string();
            state.store.create_task(&id, &t.capability, &t.brief, Some(&topic_id))?;
            task_id = Some(id);
        }
    }
    state.emit(json!({"type":"secretary","intent":decision.intent,"reply":decision.reply,"task_id":task_id,"topic_id":topic_id}));

    // ── Dispatch writer (async) ──
    if let (Some(id), Some(t)) = (task_id.clone(), decision.task.clone()) {
        if t.capability == "write" {
            let st = state.clone();
            let tid = topic_id.clone();
            let brief = t.brief.clone();
            tokio::spawn(async move {
                let _ = run_writer(st, id, tid, brief).await;
            });
        } else {
            let _ = state.store.set_task_status(&id, "failed", Some("暂不支持的能力"));
            state.emit(json!({"type":"task","task_id":id,"capability":t.capability,"status":"failed","topic_id":topic_id}));
        }
    }

    Ok(TurnResult {
        chunk_id,
        node_id,
        calibrated_text: decision.calibrated_text,
        merged: false,
        topic_id,
        secretary: Secretary {
            intent: decision.intent,
            reply: decision.reply,
            task_id,
        },
    })
}

pub async fn run_writer(state: Arc<AppState>, task_id: String, topic_id: String, brief: String) -> Result<()> {
    let _ = state.store.set_task_status(&task_id, "running", None);
    let _ = state.store.set_topic_status(&topic_id, "generating");
    state.emit(json!({"type":"task","task_id":task_id,"capability":"write","status":"running","topic_id":topic_id}));

    let material: Vec<String> = state
        .store
        .get_nodes_by_topic(&topic_id)?
        .into_iter()
        .map(|n| n.calibrated_text)
        .collect();

    match write_remote(&state.llm, &brief, &material).await {
        Ok(article) => {
            let _ = state.store.set_topic_article(&topic_id, &article, "complete");
            let head: String = article.chars().take(200).collect();
            let _ = state.store.set_task_status(&task_id, "done", Some(&head));
            state.emit(json!({"type":"article_done","topic_id":topic_id,"article_md":article}));
            state.emit(json!({"type":"task","task_id":task_id,"capability":"write","status":"done","topic_id":topic_id}));
            let reply = match extract_title(&article) {
                Some(t) => format!("写好了，题目叫《{t}》，在右边可以看。要我再改吗？"),
                None => "写好了，右边可以看。要我再改吗？".to_string(),
            };
            state.emit(json!({"type":"secretary","intent":"task","reply":reply,"task_id":task_id,"topic_id":topic_id}));
        }
        Err(e) => {
            eprintln!("[writer] failed on topic {topic_id}: {e}");
            let _ = state.store.set_task_status(&task_id, "failed", Some(&e.to_string()));
            let _ = state.store.set_topic_status(&topic_id, "draft");
            state.emit(json!({"type":"task","task_id":task_id,"capability":"write","status":"failed","topic_id":topic_id}));
            state.emit(json!({"type":"error","scope":"writer","message":e.to_string(),"topic_id":topic_id}));
        }
    }
    Ok(())
}

async fn write_remote(llm: &LlmConfig, brief: &str, material: &[String]) -> Result<String> {
    if llm.api_key.is_empty() {
        anyhow::bail!("ANTHROPIC_AUTH_TOKEN 未设置");
    }
    let mat = if material.is_empty() {
        "（暂无口述素材，请据任务简述写一篇合理初稿。）".to_string()
    } else {
        material
            .iter()
            .enumerate()
            .map(|(i, t)| format!("[{}] {}", i + 1, t))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user = format!("写作任务：{brief}\n\n口述素材（按时间顺序）：\n{mat}");
    let body = json!({
        "model": llm.pro_model,
        "max_tokens": 8192,
        "thinking": {"type": "disabled"},
        "system": WRITER_SYSTEM,
        "messages": [{"role": "user", "content": user}],
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages", llm.base_url))
        .header("x-api-key", &llm.api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("writer {s}: {}", &t[..t.len().min(300)]);
    }
    let v: Value = resp.json().await?;
    let mut text = String::new();
    if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
        for b in arr {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = b.get("text").and_then(|t| t.as_str()) {
                    text.push_str(s);
                }
            }
        }
    }
    Ok(text)
}

fn extract_title(md: &str) -> Option<String> {
    for line in md.lines() {
        if let Some(rest) = line.trim().strip_prefix("# ") {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}
