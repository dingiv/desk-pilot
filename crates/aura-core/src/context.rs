//! context.rs — Stage2 校准模型的滑动窗口上下文。维护最近 N 条整流记录
//! (raw + calibrated 对 + intent + 时间戳)，按需格式化为 LLM prompt 的上下文块。
//!
//! 设计要点：
//! - 存 (raw, calibrated) 对而非仅存 calibrated —— 让 LLM 看到用户的 ASR 错误模式
//!   （OpenLess 的 `prior_turns: &[(String, String)]` 做法）
//! - 支持两种格式化模式：
//!   1) `as_compact()` → 纯文本列表（低 token 开销，日常校准用）
//!   2) `as_pairs()`  → 原文 vs 校准对照（教 LLM 学错误模式，首次/增强校准用）
//! - 窗口满时淘汰最旧的（FIFO），容量可配

use std::collections::VecDeque;

/// 一条校准记录（一次用户说话 + 模型整流的完整对）。
#[derive(Debug, Clone)]
pub struct CalibrationEntry {
    /// Stage1 ASR 原始文本
    pub raw: String,
    /// Stage2 校准后的文本
    pub calibrated: String,
    /// 意图（chat / task），供未来 topic 切分用
    pub intent: String,
    /// Unix 时间戳（毫秒）
    pub timestamp_ms: u64,
}

/// Stage2 滑动窗口上下文管理器。
pub struct ContextWindow {
    entries: VecDeque<CalibrationEntry>,
    /// 最大保留条数（溢出时淘汰最旧的）
    cap: usize,
}

impl ContextWindow {
    /// 创建固定容量的窗口。
    pub fn new(capacity: usize) -> Self {
        Self { entries: VecDeque::with_capacity(capacity), cap: capacity }
    }

    /// 推入一条新记录。满容量时淘汰最旧的。
    pub fn push(&mut self, raw: &str, calibrated: &str, intent: &str) {
        if self.entries.len() >= self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back(CalibrationEntry {
            raw: raw.to_string(),
            calibrated: calibrated.to_string(),
            intent: intent.to_string(),
            timestamp_ms: now_ms(),
        });
    }

    /// 窗口内条目数。
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// 格式化为**紧凑**上下文文本（每句一行校准文本，低 token 开销）。
    /// 适合已有热词+校正策略的日常校准。
    pub fn as_compact(&self) -> String {
        self.entries
            .iter()
            .enumerate()
            .map(|(i, e)| format!("{}. {}", i + 1, e.calibrated))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 格式化为 **raw vs calibrated 对照**文本（原文在前，校准在后）。
    /// 适合首次校准或复杂场景——LLM 能看到用户的 ASR 错误模式，学习如何修正。
    pub fn as_pairs(&self) -> String {
        self.entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                format!(
                    "{}. 原文: {}\n   校准: {}",
                    i + 1,
                    e.raw,
                    e.calibrated
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 只取最近 N 条的 compact 格式。
    pub fn as_compact_last(&self, n: usize) -> String {
        let take = n.min(self.entries.len());
        self.entries
            .iter()
            .rev()
            .take(take)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .enumerate()
            .map(|(i, e)| format!("{}. {}", i + 1, e.calibrated))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_eviction() {
        let mut w = ContextWindow::new(2);
        w.push("a1", "A1", "chat");
        w.push("a2", "A2", "task");
        w.push("a3", "A3", "chat"); // evicts a1
        assert_eq!(w.len(), 2);
        let c = w.as_compact();
        assert!(c.contains("A2") && c.contains("A3"), "should contain A2 & A3, got: {c}");
        assert!(!c.contains("A1"), "A1 should be evicted");
    }

    #[test]
    fn pairs_formatting() {
        let mut w = ContextWindow::new(3);
        w.push("rost语言", "Rust语言", "task");
        w.push("B位引擎", "2D引擎", "task");
        let pairs = w.as_pairs();
        assert!(pairs.contains("rost语言") && pairs.contains("Rust语言"), "raw+calibrated pair missing");
        assert!(pairs.contains("B位引擎") && pairs.contains("2D引擎"));
    }

    #[test]
    fn compact_vs_pairs() {
        let mut w = ContextWindow::new(2);
        w.push("a", "A", "chat");
        assert!(w.as_compact().contains("A"), "compact should have calibrated text");
        assert!(w.as_pairs().contains("原文: a"), "pairs should have raw text");
    }
}
