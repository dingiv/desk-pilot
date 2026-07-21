//! voice-native — thin napi shim over the `voice-router` crate. Keeps the TS dev path
//! (`VOICE_LOCAL_ROUTER=1` → `native.ts` → this `.node`) working. All model logic lives in
//! voice-router; here we only wrap it for Node, running inference off the JS thread via AsyncTask.

use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi::{Env, Task};
use napi_derive::napi;
use audio_aura_router::RouterEngine as Inner;

/// Resident router engine exposed to Node. Model loaded once in `load`, kept warm.
#[napi]
pub struct RouterEngine {
    inner: Arc<Inner>,
}

#[napi]
impl RouterEngine {
    #[napi(factory)]
    pub fn load(model_dir: String, model_file: String) -> Result<RouterEngine> {
        let inner = Inner::load(&model_dir, &model_file).map_err(err)?;
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
        self.inner
            .route_blocking(&self.raw_text, self.context.as_deref(), &[])
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
