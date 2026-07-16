//! routes.rs — axum Router + handlers. Same API surface as the TS backend so devtools-web is
//! unchanged (points at this daemon via VITE_API_BASE). SSE rebroadcasts the pipeline event bus.

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tower_http::cors::CorsLayer;

use crate::{audio, pipeline, AppState};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/stream", get(stream))
        .route("/api/turn", post(turn))
        .route("/api/dev/inject-turn", post(inject_turn))
        .route("/api/topics", get(list_topics).post(create_topic))
        .route("/api/topics/{id}", get(get_topic).patch(patch_topic))
        .route("/api/topics/{id}/generate", post(generate))
        .route("/api/chunks/{id}/audio", get(chunk_audio))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({"ok": true, "mode": "daemon"}))
}

async fn stream(
    State(st): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = st.bus.subscribe();
    let hello = tokio_stream::once(Ok::<_, Infallible>(
        Event::default().data(json!({"type": "hello"}).to_string()),
    ));
    let live = BroadcastStream::new(rx).map(|res: Result<Value, _>| match res {
        Ok(v) => Ok(Event::default().data(v.to_string())),
        Err(_) => Ok(Event::default().comment("lagged")),
    });
    Sse::new(hello.chain(live)).keep_alive(KeepAlive::default())
}

async fn turn(State(st): State<Arc<AppState>>, Json(b): Json<Value>) -> Response {
    let raw = b.get("raw_text").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if raw.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "raw_text required"}))).into_response();
    }
    let input = pipeline::TurnInput {
        raw_text: raw,
        start_time: b.get("start_time").and_then(|v| v.as_i64()),
        end_time: b.get("end_time").and_then(|v| v.as_i64()),
        topic_id: b.get("topic_id").and_then(|v| v.as_str()).map(String::from),
        audio_base64: b.get("audio_base64").and_then(|v| v.as_str()).map(String::from),
        audio_mime: b.get("audio_mime").and_then(|v| v.as_str()).map(String::from),
        duration_ms: b.get("duration_ms").and_then(|v| v.as_i64()),
    };
    match pipeline::handle_turn(st, input).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => err500(e),
    }
}

async fn inject_turn(State(st): State<Arc<AppState>>, Json(b): Json<Value>) -> Response {
    let raw = b.get("raw_text").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if raw.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "raw_text required"}))).into_response();
    }
    let input = pipeline::TurnInput {
        raw_text: raw,
        start_time: None,
        end_time: None,
        topic_id: b.get("topic_id").and_then(|v| v.as_str()).map(String::from),
        audio_base64: None,
        audio_mime: None,
        duration_ms: None,
    };
    match pipeline::handle_turn(st, input).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => err500(e),
    }
}

async fn list_topics(State(st): State<Arc<AppState>>) -> Response {
    match st.store.list_topics() {
        Ok(topics) => Json(json!({"topics": topics})).into_response(),
        Err(e) => err500(e),
    }
}

async fn create_topic(State(st): State<Arc<AppState>>, Json(b): Json<Value>) -> Response {
    let title = b
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("未命名话题")
        .to_string();
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = st.store.create_topic(&id, &title) {
        return err500(e);
    }
    match st.store.get_topic(&id) {
        Ok(Some(t)) => Json(json!({"topic": t})).into_response(),
        _ => err500(anyhow::anyhow!("create failed")),
    }
}

async fn get_topic(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match st.store.get_topic(&id) {
        Ok(Some(topic)) => {
            let nodes = st.store.get_nodes_by_topic(&id).unwrap_or_default();
            Json(json!({"topic": topic, "nodes": nodes})).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"error": "topic not found"}))).into_response(),
        Err(e) => err500(e),
    }
}

async fn patch_topic(State(st): State<Arc<AppState>>, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    let title = match b.get("title").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return (StatusCode::BAD_REQUEST, Json(json!({"error": "title required"}))).into_response(),
    };
    if st.store.get_topic(&id).ok().flatten().is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "topic not found"}))).into_response();
    }
    let _ = st.store.update_topic_title(&id, &title);
    match st.store.get_topic(&id) {
        Ok(Some(t)) => Json(json!({"topic": t})).into_response(),
        _ => err500(anyhow::anyhow!("update failed")),
    }
}

async fn generate(State(st): State<Arc<AppState>>, Path(id): Path<String>, Json(b): Json<Value>) -> Response {
    if st.store.get_topic(&id).ok().flatten().is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "topic not found"}))).into_response();
    }
    let brief = b
        .get("brief")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("把本话题的口述素材整理成一篇结构化文章")
        .to_string();
    let task_id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = st.store.create_task(&task_id, "write", &brief, Some(&id)) {
        return err500(e);
    }
    let st2 = st.clone();
    let tid = id.clone();
    let bid = task_id.clone();
    tokio::spawn(async move {
        let _ = pipeline::run_writer(st2, bid, tid, brief).await;
    });
    Json(json!({"task_id": task_id})).into_response()
}

async fn chunk_audio(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match st.store.get_chunk(&id) {
        Ok(Some(c)) => match c.audio_path {
            Some(p) => match audio::read_audio(&p).await {
                Ok(bytes) => {
                    let mime = c.audio_mime.unwrap_or_else(|| "audio/webm".to_string());
                    ([(header::CONTENT_TYPE, mime)], bytes).into_response()
                }
                Err(_) => (StatusCode::NOT_FOUND, "no audio").into_response(),
            },
            None => (StatusCode::NOT_FOUND, "no audio for this chunk").into_response(),
        },
        _ => (StatusCode::NOT_FOUND, "chunk not found").into_response(),
    }
}

fn err500<E: std::fmt::Display>(e: E) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
}
