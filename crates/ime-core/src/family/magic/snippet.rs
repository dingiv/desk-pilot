//! SnippetMember — 片段作为「空名魔法命令」。
//!
//! 触发名是空串 `''`:语法 `#/<name>?<params>`,即 `#` 紧跟 `/` 进入本命令,
//! `/hello` 是路径(首段 = 片段名),`?name=Mike` 是查询(注入模板变量)。
//!
//! ```text
//! #/hello?name=Mike  →  模板 "Hello, my name is $name." 展开 → "Hello, my name is Mike."
//! ```
//!
//! 与 [`VoiceMember`](super::VoiceMember) 同模式:命令触发后 `/hello?name=Mike`
//! 逐键积累进 `arg`,每次积累后 `CommandArgs::parse`;`path[0]` 查片段注册表,
//! `query` 作为 `$var` 注入模板(经 [`crate::Expander::expand_with_vars`])。

use std::sync::Arc;

use super::member::{is_arg_char, CommandArgs, MagicMember, MemberAction};
use super::MagicResources;
use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};

/// 空名片段命令(`#/hello?name=Mike`)。
pub struct SnippetMember {
    resources: Arc<MagicResources>,
    /// 触发后累积的参数原始串(`/hello?name=Mike`)。
    arg: String,
    /// `arg` 的解析结果。
    args: CommandArgs,
    /// 已解析出的展开文本(可提交);未知片段/未知变量时为 None。
    full: Option<String>,
    /// 展开文本里 `$CURSOR` 落点(字节偏移);无 `$CURSOR` 时为 None。
    cursor: Option<usize>,
}

impl SnippetMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        SnippetMember {
            resources,
            arg: String::new(),
            args: CommandArgs::default(),
            full: None,
            cursor: None,
        }
    }

    /// 片段名 = 路径首段(`/hello` → `hello`)。
    fn name_arg(&self) -> String {
        self.args.path.first().cloned().unwrap_or_default()
    }

    /// 查表 + 展开,重建候选视图。返回可提交文本(未知片段/变量 → None)。
    fn resolve(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> ImeView {
        self.args = CommandArgs::parse(&self.arg);
        let name = self.name_arg();
        let template = self.resources.snippets.lock().unwrap().get(&name).cloned();
        let (candidate, full, cursor): (String, Option<String>, Option<usize>) = match template {
            Some(tpl) => {
                let vars: Vec<(String, String)> = self.args.query.clone();
                match env.expander().expand_with_vars(&tpl, &vars) {
                    Ok((text, cursor)) => (preview_or(&text, "…"), Some(text), cursor),
                    Err(e) => {
                        tracing::warn!(error = %e, snippet = %name, "snippet expand failed");
                        (format!("片段展开失败: {e}"), None, None)
                    }
                }
            }
            None => (format!("未知片段 /{name}"), None, None),
        };
        self.full = full;
        self.cursor = cursor;
        sm.candidates = vec![candidate];
        sm.candidates_fresh = true;
        sm.candidate_highlight = 0;
        sm.candidate_page = 0;
        sm.preedit = format!("#{}", self.arg);
        sm.cursor = sm.preedit.len();
        sm.make_view()
    }
}

/// 展开文本做展示预览:空文本给占位,否则原文(前端各自截断行)。
fn preview_or(text: &str, empty_placeholder: &str) -> String {
    if text.is_empty() { empty_placeholder.into() } else { text.to_string() }
}

impl MagicMember for SnippetMember {
    fn name(&self) -> &'static str {
        "" // 空名 —— 片段命令无 `#name` 触发名
    }

    fn description(&self) -> &'static str {
        "snippet" // 仅诊断用;hints 里被跳过
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__SNIPPET__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(SnippetMember::new(Arc::clone(&self.resources)))
    }

    fn activate(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> ImeView {
        self.resolve(sm, env)
    }

    fn on_key(&mut self, sm: &mut StateMachine, ch: char, env: &dyn StepEnv) -> MemberAction {
        // 参数积累(与 VoiceMember 同规则):`/` 或 `?` 开启;开启后参数字符
        // 一路积累到终止键。
        if self.arg.is_empty() && (ch == '/' || ch == '?') {
            self.arg.push(ch);
            return MemberAction::View(Box::new(self.resolve(sm, env)));
        }
        if !self.arg.is_empty() && is_arg_char(ch) {
            self.arg.push(ch);
            return MemberAction::View(Box::new(self.resolve(sm, env)));
        }

        match ch {
            ' ' => match (self.full.take(), self.cursor.take()) {
                (Some(text), Some(cursor)) => MemberAction::CommitAt(text, cursor),
                (Some(text), None) => MemberAction::Commit(text),
                (None, _) => MemberAction::Commit(String::new()), // 未知片段/变量 → 空提交
            },
            '\n' | '\r' => MemberAction::Commit(format!("#{}", self.arg)),
            '\x1b' => MemberAction::Exit,
            '\x08' => {
                if !self.arg.is_empty() {
                    self.arg.pop();
                    MemberAction::View(Box::new(self.resolve(sm, env)))
                } else {
                    MemberAction::Exit
                }
            }
            _ => MemberAction::View(Box::new(StateMachine::passthrough_view())),
        }
    }

    fn tick(&mut self, _sm: &mut StateMachine, _env: &dyn StepEnv) -> Option<ImeView> {
        None // 片段是纯同步展开,无异步刷新
    }

    fn candidate_texts(&self, sm: &StateMachine) -> Vec<String> {
        match &self.full {
            Some(text) => vec![text.clone()],
            None => sm.candidates.clone(),
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
