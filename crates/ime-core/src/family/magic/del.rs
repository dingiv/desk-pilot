//! DelMember — `#del`: delete already-typed text in the application text box.
//!
//! ```text
//! #del      → 选项:[del_len=上次提交字数, 10, 20]  选中即删对应字数
//! #del/15   → 选项:[删除 15 个字符]               选中即删 15 个
//! ```
//!
//! 选中删除选项后,引擎产出 `ImeView.delete_count`(不提交文本),前端
//! (fcitx5)对每个字符 forwardKey(BackSpace) 或 deleteSurroundingText。

use std::sync::Arc;

use super::member::{CommandArgs, MagicMember, Prediction};
use super::MagicResources;
use super::FamilyEnv;

/// 固定删除长度选项。
const FIXED_OPTIONS: &[u32] = &[10, 20, 999];

/// Live delete-text command (`#del`)。
pub struct DelMember {
    resources: Arc<MagicResources>,
}

impl DelMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        DelMember { resources }
    }

    /// 触发名之后的参数串(`#del/15` → `/15`)。
    fn args_of(input: &str) -> CommandArgs {
        let rest = input.strip_prefix("#del").unwrap_or("");
        CommandArgs::parse(rest)
    }
}

impl MagicMember for DelMember {
    fn name(&self) -> &'static str {
        "del"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__DEL__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(DelMember::new(Arc::clone(&self.resources)))
    }

    fn predict(&mut self, _ctx: usize, input: &str, _env: &dyn FamilyEnv) -> Vec<Prediction> {
        let args = Self::args_of(input);

        // `#del/15` → 删除 15 个。
        if let Some(raw) = args.path.first() {
            if let Ok(n) = raw.parse::<u32>() {
                if n > 0 {
                    return vec![Prediction::delete(n, format!("删除 {n} 个字符"))];
                }
            }
        }

        // `#del` → 选项:[上次提交字数, 10, 20]。
        let last_len = *self.resources.last_commit_len.lock().unwrap();
        let mut out = vec![];
        if last_len > 0 {
            out.push(Prediction::delete(last_len, format!("<- {}", last_len)));
        }
        for n in FIXED_OPTIONS {
            out.push(Prediction::delete(*n, format!("<- {}", n)));
        }
        out
    }

    fn tick(&mut self, ctx: usize, buffer: &str, env: &dyn FamilyEnv) -> Option<Vec<Prediction>> {
        None
    }
}
