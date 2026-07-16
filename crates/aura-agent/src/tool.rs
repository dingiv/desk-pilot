//! Tool trait — the abstraction a Stage3 LLM agent (or the desktop-pet secretary) uses to invoke
//! capabilities. A real LLM-driven agent would be given the tool `name`/`description` and emit a
//! tool-call JSON; the runtime dispatches via [`Tool::invoke`]. The daemon's rule trigger calls
//! [`AddHotwordTool`] directly to demo the closed loop without an LLM.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::capability::HotwordManager;

/// One invokable capability. `args` is a JSON object; the return is a JSON value (result/error).
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn invoke(&self, args: &Value) -> Result<Value>;
}

/// `add_hotword` tool — adds a correction hotword via a [`HotwordManager`]. Args: `{"word":"…"}`.
/// This is the Stage3 → Stage2 feedback path (the manager's store is what Stage2 reads next turn).
#[derive(Clone)]
pub struct AddHotwordTool {
    mgr: Arc<dyn HotwordManager>,
}

impl AddHotwordTool {
    pub fn new(mgr: Arc<dyn HotwordManager>) -> Self {
        Self { mgr }
    }
}

impl Tool for AddHotwordTool {
    fn name(&self) -> &str {
        "add_hotword"
    }
    fn description(&self) -> &str {
        "Add a correction hotword so Stage2 writes it correctly next time. Args: {\"word\":\"…\"}."
    }
    fn invoke(&self, args: &Value) -> Result<Value> {
        let word = args
            .get("word")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("add_hotword: missing string arg 'word'"))?;
        let added = self.mgr.add(word);
        Ok(serde_json::json!({ "added": added, "word": word }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::SharedHotwordManager;

    fn empty_mgr() -> Arc<dyn HotwordManager> {
        Arc::new(SharedHotwordManager::new(Arc::new(std::sync::Mutex::new(Vec::new()))))
    }

    #[test]
    fn add_hotword_invokes_manager() {
        let mgr = empty_mgr();
        let tool = AddHotwordTool::new(Arc::clone(&mgr));
        let out = tool.invoke(&serde_json::json!({"word":"Rust"})).unwrap();
        assert_eq!(out["added"], true);
        assert_eq!(mgr.list(), vec!["Rust".to_string()]);
        // second time → dedup
        let out2 = tool.invoke(&serde_json::json!({"word":"rust"})).unwrap();
        assert_eq!(out2["added"], false);
    }

    #[test]
    fn add_hotword_missing_arg_errors() {
        let tool = AddHotwordTool::new(empty_mgr());
        assert!(tool.invoke(&serde_json::json!({})).is_err());
    }
}
