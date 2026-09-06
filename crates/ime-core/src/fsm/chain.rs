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

// ── 链式流控(round15:Session 内的链式预测 × 魔法异步)────────────────
//
// 场景:`abc'#asr'#translate`。语音不断进入时:
// - 上游文本段(abc)**不重算** —— [`ChainFlow::cache`] 按 upstream buffer
//   内容寻址,折叠命中直接复用(编辑改 buffer 天然换 key);
// - 从语音段起的下游流水线(`#asr` → `#translate`)重新预测 —— 源指纹
//   (上游候选序列)变化即标记待刷;
// - 防抖 + 节流闸门压刷新频率:源停顿够久([`ChainFlow::DEBOUNCE_MS`])
//   才允许刷,两次刷新至少隔 [`ChainFlow::THROTTLE_MS`]。

/// 刷新决策(闸门输出)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlowDecision {
    /// 放行:重算语音段之后的流水线。
    Fire,
    /// 压制:仍在防抖/节流窗口内,记 pending 待后续 tick 补刷。
    Suppress,
    /// 源无变化:无需刷新。
    Quiet,
}

/// 链式流控状态(纯状态机;时钟由调用方注入,测试可造任意时间序列)。
#[derive(Default)]
pub(crate) struct ChainFlow {
    /// 上游折叠缓存:upstream_buf → 候选列表(内容寻址;编辑自动失效)。
    cache: std::collections::HashMap<String, Vec<String>>,
    /// 测试观测:缓存命中/未命中次数。
    pub(crate) cache_hits: usize,
    pub(crate) cache_misses: usize,
    /// 上次观察到的源指纹(变化检测的滚动基线)。
    seen: String,
    /// 上次放行刷新时刻(ms;None = 从未刷过)。
    last_refresh_ms: Option<i64>,
    /// 源最近一次变化时刻(防抖起点;None = 尚无变化)。
    source_changed_ms: Option<i64>,
    /// 节流窗口内被压制的待刷变更。
    pending: bool,
    /// 是否已建立基线(首次观察后为 true;其后才是真正的变化检测)。
    primed: bool,
}

impl ChainFlow {
    /// 防抖窗口:源更新后须静默此时长才放行(流式 partial 连发不逐字刷)。
    pub(crate) const DEBOUNCE_MS: i64 = 250;
    /// 节流窗口:两次刷新的最小间隔。
    pub(crate) const THROTTLE_MS: i64 = 150;
    /// 缓存容量上限(防膨胀;超限整体清空 —— 内容寻址,重建即可)。
    const CACHE_CAP: usize = 32;

    /// 缓存命中(命中计数)。miss 时返回 None,由调用方折叠后 [`Self::store`]。
    pub(crate) fn cached(&mut self, upstream_buf: &str) -> Option<Vec<String>> {
        match self.cache.get(upstream_buf) {
            Some(v) => {
                self.cache_hits += 1;
                Some(v.clone())
            }
            None => {
                self.cache_misses += 1;
                None
            }
        }
    }

    /// 回填折叠结果(超限清空)。
    pub(crate) fn store(&mut self, upstream_buf: &str, cands: Vec<String>) {
        if self.cache.len() >= Self::CACHE_CAP {
            self.cache.clear();
        }
        self.cache.insert(upstream_buf.to_string(), cands);
    }

    /// 闸门:汇报最新源指纹与当前时刻,决定本轮异步 tick 是否放行刷新。
    ///
    /// - **首次观察**:仅记基线(键驱预测已展示过该源)→ [`FlowDecision::Quiet`];
    /// - 指纹变化 → **重置防抖起点**(流式连发期间一直推迟,停顿才放行),
    ///   记 pending;距变化不足 [`Self::DEBOUNCE_MS`] 或距上次刷新不足
    ///   [`Self::THROTTLE_MS`] → [`FlowDecision::Suppress`];
    /// - 否则放行(pending 清除,基线已同步更新)。
    pub(crate) fn gate(&mut self, source: &str, now_ms: i64) -> FlowDecision {
        if source != self.seen {
            let first = !self.primed;
            self.primed = true;
            self.seen = source.to_string();
            if first {
                // 首次观察 = 基线(键驱路径已把该源展示给用户,不重复刷)。
                return FlowDecision::Quiet;
            }
            self.source_changed_ms = Some(now_ms);
            self.pending = true;
        }
        if !self.pending {
            return FlowDecision::Quiet;
        }
        let Some(changed) = self.source_changed_ms else {
            return FlowDecision::Quiet;
        };
        let debounced = now_ms - changed >= Self::DEBOUNCE_MS;
        let throttled = self
            .last_refresh_ms
            .is_some_and(|t| now_ms - t < Self::THROTTLE_MS);
        if debounced && !throttled {
            self.last_refresh_ms = Some(now_ms);
            self.source_changed_ms = None;
            self.pending = false;
            FlowDecision::Fire
        } else {
            FlowDecision::Suppress
        }
    }

    /// 会话复位(提交/重置后):源已消费,缓存与闸门全部归零。
    pub(crate) fn clear(&mut self) {
        self.cache.clear();
        self.cache_hits = 0;
        self.cache_misses = 0;
        self.seen.clear();
        self.last_refresh_ms = None;
        self.source_changed_ms = None;
        self.pending = false;
        self.primed = false;
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── ChainFlow 闸门:防抖 + 节流时序(时钟注入,确定性)────────────
    // 时间线示意(ms):流式连发期间一直 Suppress,停顿过 DEBOUNCE 才 Fire,
    // 两次 Fire 间隔不足 THROTTLE 时即使源已变也被压制(pending 待补刷)。
    //
    //   t=0   源"A"首次观察 → Quiet(记基线:键驱预测已展示过)
    //   t=100 源"A→B"  → Suppress(防抖起点;连发不断推迟)
    //   t=200          → Suppress(停顿仅 100ms < 250)
    //   t=360          → Fire(停顿 160ms ≥ 防抖,且无节流前科)
    //   t=400 源"B→C"  → Suppress(距上次刷新 40ms < 节流 150)
    //   t=700          → Fire(补刷:防抖早已过、节流已过期)
    //   t=800 无变化   → Quiet(面板不被打搅)
    #[test]
    fn gate_debounce_then_throttle() {
        let mut f = ChainFlow::default();
        use FlowDecision::*;
        assert_eq!(f.gate("A", 0), Quiet); // 首次观察 = 基线
        assert_eq!(f.gate("B", 100), Suppress); // 变更,防抖起点
        assert_eq!(f.gate("B", 200), Suppress); // 停顿不足
        assert_eq!(f.gate("B", 360), Fire); // 防抖过 → 放行
        assert_eq!(f.gate("C", 400), Suppress); // 新变更,节流中
        assert_eq!(f.gate("C", 700), Fire); // 窗口全过 → 补刷
        assert_eq!(f.gate("C", 800), Quiet); // 无变化
    }

    #[test]
    fn gate_first_tick_with_no_change_is_quiet() {
        let mut f = ChainFlow::default();
        // 从未有过源变更(链式刚进入,键驱预测已完成)→ tick 静默。
        assert_eq!(f.gate("same", 10_000), FlowDecision::Quiet);
    }

    // ── 缓存:内容寻址 + 命中/未命中观测 + 上限清空 ────────────────────
    #[test]
    fn cache_addressed_by_content() {
        let mut f = ChainFlow::default();
        assert_eq!(f.cached("abc"), None);
        assert_eq!(f.cache_misses, 1);
        f.store("abc", vec!["abcx".into()]);
        assert_eq!(f.cached("abc"), Some(vec!["abcx".to_string()]));
        assert_eq!(f.cache_hits, 1);
        // 内容寻址:buffer 编辑换 key,天然失效(不误命中)。
        assert_eq!(f.cached("abcd"), None);
    }

    #[test]
    fn cache_cap_clears_instead_of_growing() {
        let mut f = ChainFlow::default();
        for i in 0..64 {
            f.store(&format!("k{i}"), vec![]);
        }
        assert!(f.cache.len() <= ChainFlow::CACHE_CAP);
        // 清空后重新可存。
        f.store("fresh", vec![]);
        assert_eq!(f.cached("fresh"), Some(vec![]));
    }

    // ── clear:会话复位归零 ────────────────────────────────────────────
    #[test]
    fn clear_resets_everything() {
        let mut f = ChainFlow::default();
        assert_eq!(f.gate("A", 0), FlowDecision::Quiet); // 首次观察 = 基线
        f.store("abc", vec!["x".into()]);
        f.clear();
        assert_eq!(f.cached("abc"), None);
        // 闸门也归零:重新走全时序(基线 → 变更压制 → 防抖后放行)。
        assert_eq!(f.gate("A", 400), FlowDecision::Quiet);
        assert_eq!(f.gate("B", 500), FlowDecision::Suppress);
        assert_eq!(f.gate("B", 800), FlowDecision::Fire);
    }
}
