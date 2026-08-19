//! ClipMember — `#clip`: 输出剪贴板历史项。
//!
//! ```text
//! #clip      == #clip/0 → 倒数第 1 个剪贴板项(最近复制的)
//! #clip/1             → 倒数第 2 个
//! #clip/2             → 倒数第 3 个
//! ```
//!
//! 历史由引擎侧维护(`MagicResources::clipboard_history`,最近在前):C++ 每次
//! 按键/激活时推送当前剪贴板,引擎去重累积 —— fcitx5 clipboard 公开接口只
//! 给当前值,没有历史;引擎据此攒一个使用期内的剪贴板环。

use std::sync::Arc;

use super::member::{CommandArgs, MagicMember, Prediction};
use super::MagicResources;
use crate::state::StepEnv;

/// 剪贴板历史容量。
pub const CLIP_HISTORY_CAP: usize = 20;

/// 剪贴板历史命令(`#clip`)。
pub struct ClipMember {
    resources: Arc<MagicResources>,
}

impl ClipMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        ClipMember { resources }
    }
}

impl MagicMember for ClipMember {
    fn name(&self) -> &'static str {
        "clip"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__CLIP__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(ClipMember::new(Arc::clone(&self.resources)))
    }

    fn predict(&mut self, input: &str, _env: &dyn StepEnv) -> Vec<Prediction> {
        let raw = input.strip_prefix("#clip").unwrap_or("");
        let args = CommandArgs::parse(raw);
        let hist = self.resources.clipboard_history.lock().unwrap();
        // 无参数:`#clip` → 展示最近 4 个 clipboard item(换行转义展示、原文提交)。
        if args.path.is_empty() {
            return hist.iter()
                .filter(|t| !t.is_empty())
                .take(4)
                .map(|t| Prediction::commit_raw(escape_newlines(t), t.clone()))
                .collect();
        }
        // 有参数 `/N`:N = 倒数第 N+1 个(`#clip/0` = 最近一个)。
        let n = args.path.first()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        match hist.get(n) {
            Some(text) if !text.is_empty() => {
                vec![Prediction::commit_raw(escape_newlines(text), text.clone())]
            }
            Some(_) => vec![Prediction::interactive("剪贴板为空")],
            None => vec![Prediction::interactive(format!("剪贴板历史不足(共 {})", hist.len()))],
        }
    }
}

/// 剪贴板文本展示转义:换行显示为 `\n`(候选行/preedit 单行可读);
/// `\r\n` 归一为 `\n`,孤立 `\r` 显示为 `\r`。提交不经此函数,保持原文。
fn escape_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\\n").replace('\r', "\\r")
}
