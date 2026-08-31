//! Stage 3 后处理(round9 R2 自 family.rs 迁出):
//! 合成(scorer.merge)→ 全局调整(promote_single_letter)→ 造词单字区
//! 重排 → PanelItem 落位 → 视图组装(make_view/fill_view/rebuild_magic_view)。
//!
//! [`FamilyPipeline`] 的类型声明仍在 [`super::family`];本文件是它的
//! stage3 行为分片(同 crate 跨文件 impl)。
use super::family::{ComposeState, FamilyPipeline};
use crate::family::RankedCandidate;
use crate::frontend::{ImeView, CANDIDATE_SLOTS};
use crate::fsm::family::StepEnv;

// ── 后处理产出结构 ──────────────────────────────────────────────────────

/// 一个候选词 + 它的元数据(来源家族/成员 + 权重)。调试 meta 与
/// "提交来源判断"(英文候选不再学成自生词)共用。
#[derive(Debug, Clone)]
pub(crate) struct CandMeta {
    pub text: String,
    pub score: f64,
    pub family: &'static str,
    pub source: &'static str,
}
/// Stage 3 后处理的产出项:候选文本 + 元数据 + 部分提交标记,三者同源。
/// candidates / last_meta / partial_commit_indices 三个列表由此单点派生
/// (S2 规范化:消除重排后独立数组间的对齐假设)。
#[derive(Debug, Clone)]
pub(crate) struct PanelItem {
    pub text: String,
    pub meta: CandMeta,
    pub partial: bool,
}
// ── Stage 3 管线 ────────────────────────────────────────────────────────

impl FamilyPipeline {
    /// Stage 3 后处理:全局调整(promote_single_letter)→ 造词单字区重排
    /// → 产出与 candidates 同序的 PanelItem 序列(meta/partial 同源)。
    /// full_comp_count 经 `pending_full_comp_count` 带出(postprocess 需
    /// &mut self 读 buffer/查询家族,而 query_pinyin 的落位段统一写)。
    pub(crate) fn postprocess(
        &mut self,
        collected: Vec<crate::family::FamilyCandidates>,
        env: &dyn StepEnv,
    ) -> Vec<PanelItem> {
        // 0. 合成(×priority / 全局排序 / 跨家族去重)—— 后处理第一步。
        let ranked = env.scorer().merge(collected);
        // 1. 全局调整:单字母输入的 self/case 置顶。
        let ranked = promote_single_letter(&self.comp.buffer, ranked);
        // 2. PanelItem 化:meta 与文本同源。
        let items: Vec<PanelItem> = ranked
            .into_iter()
            .map(|c| PanelItem {
                meta: CandMeta {
                    text: c.text.clone(),
                    score: c.score,
                    family: c.family,
                    source: c.source,
                },
                text: c.text,
                partial: false,
            })
            .collect();

        // 3. Layer 3 造词单字区:内部逻辑(链式豁免/多音节判定/首音节/
        //    单字过滤)在拼音家族(compose_single_chars),壳只管调用与
        //    面板重排 —— 家族返回空(链式/单音节)即不重排,原序直出。
        {
            {
                // 词头只收**真词**(非 decomp 链:X食品 类拼接对逐字造词
                // 无价值);嵌入词典场景全 decomp 时保底收首候选(nihao 的
                // "你好"也是链,head 空会让单字区顶到槽 1,space 变部分提交)。
                let real_words: Vec<usize> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| it.meta.source != "decomp")
                    .map(|(i, _)| i)
                    .take(4)
                    .collect();
                let real_words = if real_words.is_empty() { vec![0] } else { real_words };
                let texts: Vec<String> = items.iter().map(|it| it.text.clone()).collect();
                // 单字区全量放出:merged 可超 16 槽,fill_view 按页切窗后
                // 翻页全部可达。
                let char_items: Vec<PanelItem> = env
                    .compose_single_chars(&self.comp.buffer, &self.context, &texts, 32)
                    .into_iter()
                    .map(|c| PanelItem {
                        meta: CandMeta {
                            text: c.text.clone(),
                            // 家族内 raw 分(meta 显示语义与合成分类别不同,
                            // 单字区未过 ×priority —— 翻译标注见 weight-scoring.md)
                            score: c.raw_score,
                            family: c.family,
                            source: c.source,
                        },
                        text: c.text,
                        partial: true,
                    })
                    .collect();

                if !char_items.is_empty() {
                    let head: Vec<PanelItem> = real_words
                        .iter()
                        .filter_map(|&i| items.get(i).cloned())
                        .collect();
                    let head_len = head.len();
                    // tail:其余全部(head 之外的真词与链都保留 —— 旧实现
                    // skip(max_full) 会静默丢掉词头窗口里的链)。
                    let tail: Vec<PanelItem> = items
                        .into_iter()
                        .filter(|it| !head.iter().any(|h| h.text == it.text))
                        .collect();
                    let mut out = head;
                    out.extend(char_items);
                    out.extend(tail);
                    self.pending_full_comp_count = head_len;
                    return out;
                }
            }
        }
        self.pending_full_comp_count = items.len();
        items
    }
    /// Build a view from the current state (no key processed). Used by the state
    /// machine itself and by magic members rendering their candidates.
    pub(crate) fn make_view(&self) -> ImeView {
        let mut v = ImeView::empty();
        self.fill_view(&mut v);
        v.action = crate::frontend::action::HANDLED;
        v
    }
    fn fill_view(&self, view: &mut ImeView) {
        // preedit 是应用文本框里显示的内容;多行片段(如 #/angle 三角形)含
        // 字面 `\n`,直接显示会破坏应用排版 → 转义成 `\n` 文本。光标字节偏移
        // 同步调整(每个光标前的 `\n` 多占 1 字节)。
        let (escaped, display_cursor) = Self::escape_preedit(&self.comp.preedit, self.comp.cursor);
        ImeView::set_str(&mut view.preedit_text, &escaped);
        view.preedit_cursor = display_cursor as u32;
        // ── 翻页窗口:16 槽 = 从当前页首起的滑动窗口 ──
        // view.candidates 定长(协议),但内容跟随 candidate_page 滑动 ——
        // merged 全量候选(如造词单字区 15+ 个、全量链)翻页全部可达。
        // view.candidate_highlight 同步换算成窗口内序(addon 直接用作列表
        // 光标);candidate_page 仍是页号(addon 据此算选词的全局序)。
        let page_size = self.panel.page_size.max(1);
        let start = (self.panel.page * page_size).min(self.panel.items.len());
        let window = &self.panel.items[start.min(self.panel.items.len())..];
        let n = window.len().min(CANDIDATE_SLOTS);
        for (i, text) in window.iter().take(n).enumerate() {
            ImeView::set_str(&mut view.candidates[i].text, text);
            // Mark single-char partial-commit candidates with ">" label.
            if self.panel.partial.get(start + i).copied().unwrap_or(false) {
                ImeView::set_str(&mut view.candidates[i].label, ">");
            }
            // 调试模式:候选词后附提供者与权重。
            if self.candidate_meta_enabled {
                if let Some(m) = self.panel.meta.get(start + i) {
                    let meta = format!("[{:.3} {}/{}]", m.score, m.family, m.source);
                    ImeView::set_str(&mut view.candidates[i].meta, &meta);
                }
            }
        }
        view.candidate_count = n as u32;
        view.candidate_highlight = self.panel.highlight.saturating_sub(start) as u32;
        view.candidate_page = self.panel.page as u32;
        view.candidate_page_size = self.panel.page_size as u32;
        // aux_up(候选框顶部)= **原始输入**(你打了什么),与 preedit_text(应用
        // 高亮,将提交的合成结果)严格区分。命令态显示 `#asr`,拼音态显示
        // 正在打的拼音(raw_buffer,保留大小写)。
        let raw = match self.state {
            ComposeState::Snippet => self.comp.buffer.clone(),
            ComposeState::Pinyin => self.comp.raw_buffer.clone(),
            ComposeState::Idle => String::new(),
        };
        ImeView::set_str(&mut view.aux_up, &raw);
    }
    /// 从 `magic_predictions` / `magic_hints` 重建候选列表 + preedit + 视图。
    /// 候选 = [预测…, 补全…, rollback];preedit = 首条预测(精确)否则输入。
    pub(crate) fn rebuild_magic_view(&mut self) -> ImeView {
        let mut cands: Vec<String> = Vec::new();
        for p in &self.magic.predictions {
            cands.push(p.text.clone());
        }
        for h in &self.magic.hints {
            cands.push(h.clone());
        }
        // 参数输入态的裸提交候选文本 == 缓冲,不重复追加 rollback。
        let is_submit = self.magic.predictions.first().map(|p| p.submit).unwrap_or(false);
        if !is_submit {
            cands.push(self.comp.buffer.clone()); // rollback — 最后一项
        }
        self.panel.items = cands;
        self.panel.fresh = true;
        self.panel.highlight = 0;
        self.panel.page = 0;
        self.panel.full_comp_count = self.panel.items.len();
        self.panel.partial = vec![false; self.panel.items.len()];
        if let Some(head) = self.magic.predictions.first() {
            // preedit 用选项独立的预览文本(默认=展示文本)—— 允许候选行展示
            // 精简结果、文本框给完整预览。
            self.comp.preedit = head.preedit_value().to_string();
        } else {
            self.comp.preedit = self.comp.buffer.clone();
        }
        self.comp.cursor = self.comp.preedit.len();
        self.make_view()
    }}

// ── 全局调整 ────────────────────────────────────────────────────────────

/// 单字母输入:字母本尊 + 大小写互换置顶(english family 的 self/case 成员)。
/// 位置规则而非分数竞争 —— 跨家族打分下 english(×priority 0.70)赢不了
/// 拼音单音节(如 啊 ~0.86),但单字母的意图几乎总是字母本身。
///
/// `query_pinyin`(状态机候选)与 `candidates_detailed`(engine 层重排镜像)
/// 共用此规则,保持两处候选顺序一致。多字符 buffer 原样返回。
pub(crate) fn promote_single_letter(
    buffer: &str,
    mut ranked: Vec<RankedCandidate>,
) -> Vec<RankedCandidate> {
    if buffer.len() != 1 {
        return ranked;
    }
    let Some(ch) = buffer
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphabetic())
    else {
        return ranked;
    };
    let (lower, upper) = (ch.to_string(), ch.to_ascii_uppercase().to_string());
    let is_letter = |r: &RankedCandidate| {
        r.family == "english" && (r.text == lower || r.text == upper)
    };
    let mut head: Vec<_> = ranked.iter().filter(|r| is_letter(r)).cloned().collect();
    head.sort_by_key(|r| if r.text == lower { 0 } else { 1 });
    if head.is_empty() {
        return ranked;
    }
    ranked.retain(|r| !is_letter(r));
    head.extend(ranked);
    head
}