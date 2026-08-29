//! rules — the in-process **Stage3 rule trigger** (临时闭环演示;desktop-pet 秘书调度器
//! 接管前的占位)。从 daemon main.rs 迁入(2026-08-18):Stage3 触发逻辑本就属 Stage3
//! 能力层,daemon 只负责在 ParagraphCalibration 时调用它。
//!
//! 策略:从整流后的文本提取大写拉丁专名候选,作为热词加入 store —— 把 Stage2 的纠偏
//! 结果"锁进"后续轮次(在表术语解码更稳)。

use serde_json::json;
use tracing::{info, instrument};

use crate::tool::{AddHotwordTool, Tool};

/// Extract uppercase-latin proper-noun candidates from the calibrated text and add them as
/// hotwords — locking in Stage2's corrections so future turns are reinforced. Concatenation
/// artifacts ("APIdocker" — batch ASR gluing adjacent terms) are rejected so they can't
/// pollute the store. Returns whether any word was added (caller bumps the control-plane
/// version on true).
#[instrument(skip(tool))]
pub fn stage3_rule_trigger(tool: &AddHotwordTool, text: &str) -> bool {
    let mut added_any = false;
    for tok in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.len() < 2 || !tok.chars().any(|c| c.is_ascii_uppercase()) || looks_like_concat(tok)
        {
            continue;
        }
        if let Ok(out) = tool.invoke(&json!({ "word": tok })) {
            if out["added"].as_bool() == Some(true) {
                info!(word = %tok, "stage3 规则触发器加词");
                added_any = true;
            }
        }
    }
    added_any
}

/// A concatenation artifact like "APIdocker": an UPPER-UPPER-lower trigram marks the glue seam
/// (the standard camelCase word-split rule). Legit tokens survive — "GitHub" (single-cap
/// boundaries), "README" (all caps, no lower after), "Rust" (TitleCase).
pub fn looks_like_concat(tok: &str) -> bool {
    let c: Vec<char> = tok.chars().collect();
    c.windows(3)
        .any(|w| w[0].is_ascii_uppercase() && w[1].is_ascii_uppercase() && w[2].is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::looks_like_concat;

    #[test]
    fn concat_seam_rejected_legit_tokens_pass() {
        // Glue seams (UPPER-UPPER-lower trigram) — the "APIdocker" class.
        assert!(looks_like_concat("APIdocker"));
        assert!(looks_like_concat("PDFmarkdown"));
        assert!(looks_like_concat("APIs")); // plural junk, acceptable loss
        // Legit proper nouns survive.
        assert!(!looks_like_concat("Rust"));
        assert!(!looks_like_concat("GitHub")); // single-cap boundaries
        assert!(!looks_like_concat("README")); // all caps, no lower after
        assert!(!looks_like_concat("PDF"));
    }
}
