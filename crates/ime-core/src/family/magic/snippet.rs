//! SnippetMember — 片段作为「空名魔法命令」的预测提供者。
//!
//! 触发名是空串 `''`:语法 `#/<name>?<params>`,即 `#` 紧跟 `/` 进入本命令,
//! `/hello` 是路径(首段 = 片段名),`?name=Mike` 是查询(注入模板变量)。
//!
//! ```text
//! #/hello?name=Mike  →  模板 "Hello, my name is $name." 展开 → "Hello, my name is Mike."
//! ```
//!
//! `predict` 解析整段输入,查片段注册表 + 展开,返回 [展开结果](选中即上屏,
//! `$CURSOR` 落点由 prediction.cursor 携带)。未知片段/展开失败返回交互式
//! 提示(选中不上屏)。

use std::sync::Arc;

use super::member::{CommandArgs, MagicMember, Prediction};
use super::MagicResources;
use crate::state::StepEnv;

/// 空名片段命令(`#/hello?name=Mike`)。
pub struct SnippetMember {
    resources: Arc<MagicResources>,
}

impl SnippetMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        SnippetMember { resources }
    }
}

impl MagicMember for SnippetMember {
    fn name(&self) -> &'static str {
        "" // 空名 —— 片段命令无 `#name` 触发名
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__SNIPPET__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(SnippetMember::new(Arc::clone(&self.resources)))
    }

    fn predict(&mut self, input: &str, env: &dyn StepEnv) -> Vec<Prediction> {
        // 输入形如 "#/hello?name=Mike" → 剥掉 `#` 后解析 `/path` 与 `?query`。
        let raw = input.strip_prefix('#').unwrap_or(input);
        let args = CommandArgs::parse(raw);
        let name = args.path.first().cloned().unwrap_or_default();
        let template = self.resources.snippets.lock().unwrap().get(&name).cloned();

        match template {
            Some(tpl) => {
                let vars: Vec<(String, String)> = args.query.clone();
                match env.expander().expand_with_vars(&tpl, &vars) {
                    Ok((text, cursor)) => vec![Prediction { text, interactive: false, cursor }],
                    Err(e) => {
                        tracing::warn!(error = %e, snippet = %name, "snippet expand failed");
                        vec![Prediction::interactive(format!("片段展开失败: {e}"))]
                    }
                }
            }
            None => vec![Prediction::interactive(format!("未知片段 /{name}"))],
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn name_is_empty_and_token_set() {
        let fam = crate::family::magic::MagicFamily::new();
        let m = fam.spawn("__SNIPPET__").expect("snippet member spawns");
        assert_eq!(m.name(), "", "empty trigger name");
    }
}
