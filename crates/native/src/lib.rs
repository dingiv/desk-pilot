//! voice-native — thin napi shim over `audio-aura-core`'s `Calibrator` (the merged
//! 整流+路由 engine, formerly the `voice-router` crate). Keeps the TS dev path
//! (`VOICE_LOCAL_ROUTER=1` → `native.ts` → this `.node`) working. All model logic lives in
//! audio-aura-core; here we only wrap it for Node, running inference off the JS thread via
//! AsyncTask.

use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi::{Env, Task};
use napi_derive::napi;
use audio_aura_core::Calibrator as Inner;

/// Resident router engine exposed to Node. Talks to dp-router over HTTP (kept warm as long as
/// the connection is alive); no model weights in this process.
#[napi]
pub struct RouterEngine {
    inner: Arc<Inner>,
}

#[napi]
impl RouterEngine {
    /// `endpoint` is the dp-router base URL (e.g. `http://127.0.0.1:8080`); `model` is the
    /// server-side model name registered with dp-router (e.g. `qwen2.5-3b-instruct-q4_k_m`).
    #[napi(factory)]
    pub fn load(endpoint: String, model: String) -> Result<RouterEngine> {
        let inner = Inner::load(&endpoint, &model).map_err(err)?;
        Ok(RouterEngine {
            inner: Arc::new(inner),
        })
    }

    /// Merged 整流+路由 → raw model JSON text. Runs on the libuv threadpool (non-blocking to JS).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn route(&self, raw_text: String, context: Option<String>) -> AsyncTask<RouteTask> {
        AsyncTask::new(RouteTask {
            inner: Arc::clone(&self.inner),
            raw_text,
            context,
        })
    }
}

pub struct RouteTask {
    inner: Arc<Inner>,
    raw_text: String,
    context: Option<String>,
}

impl Task for RouteTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<String> {
        // aura 的 Calibrator 封装层持有 dp_models::http::HttpLlm(连 dp-router),
        // 并保留 calibrate_blocking (PromptBuilder 组装 + infer) 作为 Stage2 便捷入口。
        self.inner
            .calibrate_blocking(&self.raw_text, self.context.as_deref(), &[])
            .map_err(err)
    }

    fn resolve(&mut self, _env: Env, output: String) -> Result<String> {
        Ok(output)
    }
}

fn err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

#[napi]
pub fn hello(name: String) -> String {
    format!("voice-native (Rust) alive — hello {name}")
}
