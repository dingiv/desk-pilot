//! SnippetMember — 片段作为「空名魔法命令」的预测提供者。
//!
//! 触发名是空串 `''`:语法 `#/<name><path>?<params>`,即 `#` 紧跟 `/` 进入本命令,
//! `/hello` 是路径(首段 = 片段名),`?name=Mike` 是查询(注入模板变量)。
//!
//! ```text
//! #/hello?name=Mike  →  模板 "Hello, my name is $name." 展开 → "Hello, my name is Mike."
//! #/angle/O          →  模板里的 ${PATH_VAR} 用路径参数 `O` 填充
//! #/env/HOME         →  ${ENV:USERNAME} 读环境变量;${ENV:$PATH_VAR} 先展开
//!                        PATH_VAR 得变量名再读对应环境变量
//! ```
//!
//! 路径段:
//! - 首段 = 片段名(`/hello`);
//! - 其余路径段拼接为 `${PATH_VAR}`(用 `/` 连接);
//! - `?name=Mike` 查询参数作为同名模板变量。
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

    fn predict(&mut self, _ctx: usize, input: &str, env: &dyn StepEnv) -> Vec<Prediction> {
        // 输入形如 "#/hello?name=Mike" → 剥掉 `#` 后解析 `/path` 与 `?query`。
        let raw = input.strip_prefix('#').unwrap_or(input);
        let args = CommandArgs::parse(raw);
        let name = args.path.first().cloned().unwrap_or_default();
        let entry = self.resources.snippets.lock().unwrap().get(&name).cloned();

        match entry {
            Some(entry) => {
                // 查询参数作为模板变量;路径剩余段拼接成 PATH_VAR(用 `/`)。
                let mut vars: Vec<(String, String)> = args.query.clone();
                if args.path.len() > 1 {
                    let path_var = args.path[1..].join("/");
                    vars.push(("PATH_VAR".to_string(), path_var));
                }
                match env.expander().expand_with_vars(&entry.template, &vars) {
                    Ok((text, cursor)) => {
                        // 候选行显示 `comment`(缺省显示展开文本);上屏用展开结果。
                        let display = if entry.comment.is_empty() {
                            text.clone()
                        } else {
                            entry.comment.clone()
                        };
                        let mut p = Prediction::commit_triple(display, text.clone(), text);
                        p.cursor = cursor;
                        vec![p]
                    }
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
