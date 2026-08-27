//! chain — 链式预测的段解析(纯函数,无状态)。
//!
//! `'` 是链分隔符(结构字符):输入串按它切成若干段,每段独立路由 ——
//! `#` 开头的段是**命令链**(Magic),其余是**文本链**(拼音/英文,可再含
//! `'` 交由拼音家族组合)。链结构完全由 buffer 内容决定,backspace 删 `'`
//! 天然回退,无隐藏状态。
//!
//! ```text
//! ti'an              → [Text("ti"), Text("an")]                P0 组合
//! mingtian'#tr       → [Text("mingtian"), Command("#tr")]      P1 上下文
//! #clip/1'#tr        → [Command("#clip/1"), Command("#tr")]    源 → 变换
//! X'#tr'#upper       → [Text, Command("#tr"), Command("#upper")] 左折叠级联
//! ```

/// 一条链段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainSeg {
    /// 文本链(拼音/英文;内部可含 `'` 由拼音家族组合)。
    Text(String),
    /// 命令链(`#cmd/args?query`)。
    Command(String),
}

/// 把 buffer 按 `'` 切成段。空段(连续 `'`、首尾 `'`)产生 `Text("")` —
/// P2 的空链(整页上下文)语义在此占位,当前调用方忽略空文本段。
pub fn split_segments(buffer: &str) -> Vec<ChainSeg> {
    buffer
        .split('\'')
        .map(|seg| {
            if seg.starts_with('#') {
                ChainSeg::Command(seg.to_string())
            } else {
                ChainSeg::Text(seg.to_string())
            }
        })
        .collect()
}

/// 是否处于**链式命令模式**:存在 `'` 分隔且最后一段是命令(用户正在输入
/// /编辑命令链)。上游折叠求值只在这种模式下发生;纯文本链(`ti'an`)
/// 留在拼音组合路径(P0)。
pub fn is_chain_command(buffer: &str) -> bool {
    if !buffer.contains('\'') {
        return false;
    }
    match buffer.rsplit_once('\'') {
        Some((_, last)) => last.starts_with('#') && buffer.rsplit('\'').count() >= 2,
        None => false,
    }
}

/// 重组段列表为 buffer 形态(段间以 `'` 连接)—— 上游前缀的还原。
pub fn join_segments(segs: &[ChainSeg]) -> String {
    segs.iter()
        .map(|s| match s {
            ChainSeg::Text(t) | ChainSeg::Command(t) => t.as_str(),
        })
        .collect::<Vec<_>>()
        .join("'")
}
