//! Stage 3 后处理(round9 R2 自 family.rs 迁出):
//! 合成(scorer.merge)→ 全局调整(promote_single_letter)→ 造词单字区
//! 重排 → PanelItem 落位 → 视图组装(make_view/fill_view/rebuild_magic_view)。
//!
//! [`StateMachine`] 的类型声明仍在 [`super::family`];本文件是它的
//! stage3 行为分片(同 crate 跨文件 impl)。
use super::family_prediction::ComposeState;
use crate::family::RankedCandidate;
use crate::frontend::{ImeView, CANDIDATE_SLOTS};
use crate::fsm::family_prediction::{apply_input_casing, StepEnv};
use std::collections::HashMap;

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
// ── Stage 3 候选过滤框架(round10 W7,骨架)────────────────────────────
//
// 池子放开(top_n/视图槽)后,泛滥候选的收敛点在 stage3。过滤是**链式**
// 的:合成(merge)之后、置顶/单字区重排之前按序过链 —— Drop 即移除、
// Demote 调分,链跑完统一重排。链空(默认)时零成本直通,行为不变。
//
// 加一个过滤器 = 一个 struct 实现 [`CandidateFilter`] + 构造时
// `ImeEngine::add_filter`(参考下方 doc 示例);不动管线。
//
// ```ignore
// struct DropRareEnglish { floor: f64 }
// impl CandidateFilter for DropRareEnglish {
//     fn name(&self) -> &'static str { "drop_rare_english" }
//     fn judge(&self, c: &RankedCandidate, _ctx: &FilterCtx) -> Verdict {
//         if c.family == "english" && c.score < self.floor { Verdict::Drop }
//         else { Verdict::Keep }
//     }
// }
// engine.add_filter(Box::new(DropRareEnglish { floor: 0.3 }));
// ```

/// 单候选的过滤裁决。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// 放行,原分原序。
    Keep,
    /// 放行但改为给定分数(链尾统一重排;绝对新分,不是乘系数 —— 可读、
    /// 可比、调试 meta 直接可见)。
    Demote(f64),
    /// 丢弃,不进面板。
    Drop,
}

/// 过滤判据的只读快照(逐步扩充;不暴露 pipeline 可变内部 —— 过滤器
/// 必须无副作用,排序/落位由管线统一负责)。
#[derive(Debug, Clone, Copy)]
pub struct FilterCtx<'a> {
    /// 当前输入缓冲(Snippet 态含命令文本;Pinyin 态为纯拼音)。
    pub buffer: &'a str,
    /// 管线状态(过滤规则可按态分化,如 Idle 不滤、Pinyin 滤长尾)。
    pub state: ComposeState,
    /// 该候选在过滤前的全局序(0 起;以原序为准,过滤不回填)。
    pub rank: usize,
}

/// 一个 stage3 过滤器。纯函数语义:同一输入判同一裁决,不改外部状态。
pub trait CandidateFilter: Send + Sync {
    /// 过滤器标识(调试日志/将来 yaml 开关的键)。
    fn name(&self) -> &'static str;
    fn judge(&self, cand: &RankedCandidate, ctx: &FilterCtx) -> Verdict;
}

/// 有序过滤器链。顺序即语义:先注册先判,首个 `Drop` 生效;
/// `Demote` 覆盖分数(后判者以前者结果为准)。链跑完可选执行
/// **池顶截断**(`pool_cap` —— 池数自适应收紧,见 [`FilterChain::set_pool_cap`])。
#[derive(Default)]
pub struct FilterChain {
    filters: Vec<Box<dyn CandidateFilter>>,
    /// 全局池顶:过滤+重排后保留前 N 条(按分)。`None` = 不限。
    pool_cap: Option<usize>,
    /// 家族配额:单家族进池上限。纯分数池顶挡不住"一家灌满"(简拼深池
    /// 96 条全在 0.38~0.46 窄带,把 emoji/英文挤出任意分数线)—— 配额
    /// 才是"泛滥"的正解:每家族保代表性的前 N 条,其余让位。
    quota_per_family: Option<usize>,
}

/// 共享空链(StepEnv 默认实现返回它 —— 未注册过滤器的引擎零开销)。
pub static EMPTY_FILTERS: FilterChain = FilterChain {
    filters: Vec::new(),
    pool_cap: None,
    quota_per_family: None,
};

impl FilterChain {
    /// 出厂防洪配置:各家族绝对分数底线 + 家族配额 + 全局池顶。引擎构造
    /// 默认注册;想换策略就自组(或 add_filter 叠加)。
    pub fn with_flood_control(floors: crate::family::scoring::ScoreFloors) -> Self {
        let mut chain = FilterChain::default();
        chain.push(Box::new(ScoreFloorFilter {
            floors: HashMap::from([
                ("pinyin", floors.pinyin),
                ("english", floors.english),
                ("emoji", floors.emoji),
            ]),
            default_floor: floors.default,
        }));
        chain.set_quota_per_family(Some(DEFAULT_FAMILY_QUOTA));
        chain.set_pool_cap(Some(DEFAULT_POOL_CAP));
        chain
    }

    pub fn push(&mut self, f: Box<dyn CandidateFilter>) {
        self.filters.push(f);
    }

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    pub fn len(&self) -> usize {
        self.filters.len()
    }

    /// 池数自适应(pool cap):过滤+重排后仅保留前 `n` 条。池子比视图
    /// (48 槽)还深时,尾部候选翻页价值趋零,直接在 stage3 收掉。
    /// `None` = 不限。
    pub fn set_pool_cap(&mut self, n: Option<usize>) {
        self.pool_cap = n.filter(|v| *v > 0);
    }

    /// 家族配额:每家族进池上限(代表性截断)。`None` = 不限。
    pub fn set_quota_per_family(&mut self, n: Option<usize>) {
        self.quota_per_family = n.filter(|v| *v > 0);
    }

    /// 跑链:按序判每条候选,Drop 移除、Demote 调分;发生过调分则链尾
    /// 按 score 降序 stable 重排(同分保持原相对序);然后家族配额、
    /// 全局池顶。空链且无池顶/配额时直通零分配。
    pub fn run(
        &self,
        ranked: Vec<RankedCandidate>,
        buffer: &str,
        state: ComposeState,
    ) -> Vec<RankedCandidate> {
        if self.filters.is_empty() && self.pool_cap.is_none() && self.quota_per_family.is_none() {
            return ranked;
        }
        let mut demoted = false;
        let mut kept: Vec<RankedCandidate> = Vec::with_capacity(ranked.len());
        for (rank, mut cand) in ranked.into_iter().enumerate() {
            let ctx = FilterCtx { buffer, state, rank };
            let mut drop = false;
            for f in &self.filters {
                match f.judge(&cand, &ctx) {
                    Verdict::Keep => {}
                    Verdict::Demote(new) => {
                        if new != cand.score {
                            cand.score = new;
                            demoted = true;
                        }
                    }
                    Verdict::Drop => {
                        drop = true;
                        break;
                    }
                }
            }
            if !drop {
                kept.push(cand);
            }
        }
        if demoted {
            kept.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        // 家族配额(kept 已按分序,retain 保序):每家族只留代表性前 N。
        if let Some(q) = self.quota_per_family {
            let mut per: HashMap<&str, usize> = HashMap::new();
            kept.retain(|c| {
                let n = per.entry(c.family).or_insert(0);
                *n += 1;
                *n <= q
            });
        }
        // 全局池顶兜底。
        if let Some(n) = self.pool_cap {
            kept.truncate(n);
        }
        kept
    }
}

/// 全局池顶默认值:对齐单家族最大输出(large_dict_take=96)——
/// 保证任何家族的合理候选不被池顶误截,只收真正的超深尾部。
pub const DEFAULT_POOL_CAP: usize = 96;

/// 家族配额默认值:对齐视图槽(48)—— 单家族超过一屏的候选,翻页
/// 价值趋零;配额让位给其它家族的代表性候选(cd→📀 场景)。
pub const DEFAULT_FAMILY_QUOTA: usize = 48;

/// 方案一:家族绝对分数底线。merge 后的分数 = raw × priority/100,
/// **各家族分数域不同**(english ×0.70、emoji ×0.50),统一底线必误杀
/// —— 底线按家族配置。`rank == 0` 恒放行(任何输入至少给一个候选)。
#[derive(Debug, Clone)]
pub struct ScoreFloorFilter {
    /// 家族名 → merge 后分数底线。未列出的家族用 `default_floor`。
    pub floors: HashMap<&'static str, f64>,
    pub default_floor: f64,
}

impl Default for ScoreFloorFilter {
    fn default() -> Self {
        // 底线取值(round10 实测):pinyin 垃圾长尾(lattice_prefix 深衰减
        // 后 0.11~0.17)vs 真词地板(freq_to_score 下限 0.25 / W4 条件折扣后
        // 0.19+)—— 0.18 恰在中间;english prefix 长尾 ≈0.42,0.35 仅清
        // sub-floor 噪声;emoji 前缀 0.30,0.25 保前缀。偏保守 —— 这是防洪
        // 不是清仓,收紧走配置。
        ScoreFloorFilter {
            floors: HashMap::from([("pinyin", 0.18), ("english", 0.35), ("emoji", 0.25)]),
            default_floor: 0.30,
        }
    }
}

impl CandidateFilter for ScoreFloorFilter {
    fn name(&self) -> &'static str {
        "score_floor"
    }
    fn judge(&self, cand: &RankedCandidate, ctx: &FilterCtx) -> Verdict {
        if ctx.rank == 0 {
            return Verdict::Keep; // 第一候选恒放行
        }
        let floor = self.floors.get(cand.family).copied().unwrap_or(self.default_floor);
        if cand.score < floor {
            Verdict::Drop
        } else {
            Verdict::Keep
        }
    }
}

// ── Stage 3 合成(round11 自 family/mod.rs 迁入:同阶段同一模块)────────

impl crate::family::UnifiedScorer {
    /// Stage 3 合成(后处理第一步):×priority 乘数、全局排序、跨家族去重。
    /// 输入是 stage2 `collect` 的家族收集结果 —— collect(家族内预测+预过滤)
    /// 与 merge(跨家族合成)分属两阶段,故 merge 住在 stage3 模块。
    pub fn merge(&self, collected: Vec<crate::family::FamilyCandidates>) -> Vec<RankedCandidate> {
        let mut scored: Vec<(f64, RankedCandidate)> = Vec::new();
        for fc in collected {
            for c in fc.candidates {
                let final_score = c.raw_score * fc.bonus;
                scored.push((
                    final_score,
                    RankedCandidate {
                        text: c.text,
                        score: final_score,
                        family: c.family,
                        source: c.source,
                    },
                ));
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut seen = std::collections::HashSet::new();
        scored
            .into_iter()
            .filter(|(_, rc)| seen.insert(rc.text.clone()))
            .map(|(_, rc)| rc)
            .collect()
    }
}

// ── Stage 3 管线 ────────────────────────────────────────────────────────

// ── 交付件契约(round12:stage2↔stage3 唯一交互面)────────────────────

/// 学习回执(round14):stage2 提交路径产出的**纯数据** —— stage2 不再
/// 直接调学习接口(家族不感知 commit),由 ControlPane 在事件处理后
/// 统一交 [`learn_commit`] 结算。后处理感知候选/提交的来源家族,
/// 据此分流学习。
#[derive(Debug, Clone)]
pub(crate) enum CommitReceipt {
    /// 拼音 L0 频率加成(整词提交或逐字选择)。
    PinyinPick { pinyin: String, word: String },
    /// 拼音自生词(经历过逐字选择后整体提交,无条件入单词本)。
    ComposedPhrase { pinyin: String, text: String },
    /// 提交落地:recency 按 family 分流(english → 英文 recency,其余 →
    /// 拼音 recency);纯 ASCII 字母数字且非英文族 → 学英文自生词;
    /// 提交长度统计(#del 用)。
    Commit { text: String, family: Option<&'static str> },
}

/// 学习结算(后处理职责):按回执类型分发到对应家族的学习接口。
pub(crate) fn learn_commit(
    receipt: &CommitReceipt,
    wb: &crate::store::wordbook::WordBook,
    env: &dyn StepEnv,
) {
    match receipt {
        // L0 在 inputx_pinyin 引擎词典内(wordbook 边界之外的例外),经家族句柄写。
        CommitReceipt::PinyinPick { pinyin, word } => env.record_pick(pinyin, word),
        // 自生词入本(后处理直接写单词本)。
        CommitReceipt::ComposedPhrase { pinyin, text } => {
            wb.pinyin.learn_composed_phrase(pinyin, text)
        }
        CommitReceipt::Commit { text, family } => {
            // recency 按家族分流:english → 英文册,其余 → 拼音册。
            wb.record_commit(*family == Some("english"), text);
            env.record_commit_len(text);
            // 纯 ASCII 字母数字且非英文族提交 → 学入英文册 user 层。
            if *family != Some("english")
                && !text.is_empty()
                && text.chars().all(|c| c.is_ascii_alphanumeric())
            {
                wb.english.learn_word(text);
            }
        }
    }
}

/// stage2 → stage3:只读会话快照 + 家族收集结果。
pub(crate) struct PostRequest {
    /// 组合缓冲快照(置顶/过滤链/造词区判定)。
    pub buffer: String,
    /// 上下文文本(造词区家族调用)。
    pub context: crate::family::InputContext,
    /// 会话态(过滤链分支)。
    pub state: ComposeState,
    /// 家族收集结果(未合成)。
    pub collected: Vec<crate::family::FamilyCandidates>,
}

/// stage2 → 渲染:只读面板快照(render 的唯一入参)。
pub(crate) struct PanelSnapshot {
    pub items: Vec<String>,
    pub meta: Vec<CandMeta>,
    pub partial: Vec<bool>,
    pub highlight: usize,
    pub page: usize,
    pub page_size: usize,
    pub preedit: String,
    pub cursor: usize,
    pub state: ComposeState,
    pub raw_buffer: String,
    pub buffer: String,
    pub candidate_meta: bool,
}

/// stage3 → stage2:落位回执(经 stage1 转交)。
pub(crate) struct PostOutcome {
    /// 面板全量(置顶 → 过滤链 → 造词单字区)。
    pub items: Vec<PanelItem>,
    /// 真词头边界(部分提交语义;取代 pending_full_comp_count 带出槽)。
    pub full_comp_count: usize,
}

/// 造词单字区之前的真词头窗口(见 postprocess 内注释:多音节输入时
/// 多字词优先,单字区后靠)。
const COMPOSE_HEAD_WORDS: usize = 10;

/// Stage 3 后处理纯函数(round13 四步显式化):
/// ① **家族间综合评分**(merge:×priority / 全局排序 / 跨家族去重);
/// ② **评分微调**(promote_single_letter 置顶 + FilterChain Drop/Demote);
/// ③ 造词单字区重排(compose_single_chars 注入,partial 标记);
/// ④ **输入统计与上下文构建**在别处:提交学习(record_pick/L0)、
///    上下文注入(InputContext 经 `scorer.collect` 在**预测前**交给
///    各家族,家族据此上下文感知)—— 见 family.rs / StepEnv。
/// 只吃交付件,禁止持有状态机引用(round12 宪法第 6 条)。
pub(crate) fn postprocess(req: PostRequest, env: &dyn StepEnv) -> PostOutcome {

        // 0. 合成(×priority / 全局排序 / 跨家族去重)—— 后处理第一步。
        let ranked = env.scorer().merge(req.collected);
        // 1. 全局调整:单字母输入的 self/case 置顶。
        let ranked = promote_single_letter(&req.buffer, ranked);
        // 1.5 候选过滤链(stage3 框架):Drop 移除、Demote 调分重排。
        //     空链(默认)零成本直通 —— 池子放开后泛滥候选的收敛点。
        let ranked = env.filters().run(ranked, &req.buffer, req.state);
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
        //
        //    词头只收**真词**(非 decomp 链:X食品 类拼接对逐字造词
        //    无价值);嵌入词典场景全 decomp 时保底收首候选(nihao 的
        //    "你好"也是链,head 空会让单字区顶到槽 1,space 变部分提交)。
        //    窗口取 10(修 round20 反馈:yibu → 异步被挤到 #38):
        //    多音节输入的意图首先是多字词,真词头必须整页优先于造词
        //    单字区 —— 旧窗口 4 远小于单字区 32,词头之后的全部真词
        //    (如 #5 的异步)被活埋在 32 个单字后面。10 ≈ 覆盖第一页
        //    真词 + 余量,单字区从第二页起仍可翻页直达。
        let real_words: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.meta.source != "decomp")
            .map(|(i, _)| i)
            .take(COMPOSE_HEAD_WORDS)
            .collect();
        let real_words = if real_words.is_empty() { vec![0] } else { real_words };
        let texts: Vec<String> = items.iter().map(|it| it.text.clone()).collect();
        // 单字区全量放出:merged 可超 16 槽,fill_view 按页切窗后
        // 翻页全部可达。
        let char_items: Vec<PanelItem> = env
            .compose_single_chars(&req.buffer, &req.context, &texts, 32)
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
            PostOutcome { items: out, full_comp_count: head_len }
        } else {
            let full_comp_count = items.len();
            PostOutcome { items, full_comp_count }
        }
}

/// 视图渲染纯函数(round12 S4):只吃 [`PanelSnapshot`] 快照,
/// 禁止访问任何状态机字段 —— 渲染语义与状态所有权彻底分离。
pub(crate) fn render(snap: &PanelSnapshot) -> ImeView {
    let mut view = ImeView::empty();
    fill_snapshot_view(snap, &mut view);
    view
}

/// 待提交文本纯函数:候选首条按键入原始大小写回填,无候选回退原始缓冲。
pub(crate) fn pending_commit_text(snap: &PanelSnapshot) -> String {
    snap.items
        .first()
        .map(|c| apply_input_casing(c, &snap.raw_buffer))
        .unwrap_or_else(|| snap.raw_buffer.clone())
}

fn fill_snapshot_view(snap: &PanelSnapshot, view: &mut ImeView) {
        // preedit 是应用文本框里显示的内容;多行片段(如 #/angle 三角形)含
        // 字面 `\n`,直接显示会破坏应用排版 → 转义成 `\n` 文本。光标字节偏移
        // 同步调整(每个光标前的 `\n` 多占 1 字节)。
        let (escaped, display_cursor) = escape_preedit(&snap.preedit, snap.cursor);
        ImeView::set_str(&mut view.preedit_text, &escaped);
        view.preedit_cursor = display_cursor as u32;
        // ── 翻页窗口:16 槽 = 从当前页首起的滑动窗口 ──
        // view.candidates 定长(协议),但内容跟随 candidate_page 滑动 ——
        // merged 全量候选(如造词单字区 15+ 个、全量链)翻页全部可达。
        // view.candidate_highlight 同步换算成窗口内序(addon 直接用作列表
        // 光标);candidate_page 仍是页号(addon 据此算选词的全局序)。
        let page_size = snap.page_size.max(1);
        let start = (snap.page * page_size).min(snap.items.len());
        let window = &snap.items[start.min(snap.items.len())..];
        let n = window.len().min(CANDIDATE_SLOTS);
        for (i, text) in window.iter().take(n).enumerate() {
            ImeView::set_str(&mut view.candidates[i].text, text);
            // Mark single-char partial-commit candidates with ">" label.
            if snap.partial.get(start + i).copied().unwrap_or(false) {
                ImeView::set_str(&mut view.candidates[i].label, ">");
            }
            // 调试模式:候选词后附提供者与权重。
            if snap.candidate_meta {
                if let Some(m) = snap.meta.get(start + i) {
                    let meta = format!("[{:.3} {}/{}]", m.score, m.family, m.source);
                    ImeView::set_str(&mut view.candidates[i].meta, &meta);
                }
            }
        }
        view.candidate_count = n as u32;
        view.candidate_highlight = snap.highlight.saturating_sub(start) as u32;
        view.candidate_page = snap.page as u32;
        view.candidate_page_size = snap.page_size as u32;
        // aux_up(候选框顶部)= **原始输入**(你打了什么),与 preedit_text(应用
        // 高亮,将提交的合成结果)严格区分。命令态显示 `#asr`,拼音态显示
        // 正在打的拼音(raw_buffer,保留大小写)。
        let raw = match snap.state {
            ComposeState::Snippet => snap.buffer.clone(),
            ComposeState::Pinyin => snap.raw_buffer.clone(),
            ComposeState::Idle => String::new(),
        };
        ImeView::set_str(&mut view.aux_up, &raw);
    }

// ── 视图 helper(round11:全管线单点,禁止内联重写;round12 S4 降为
// 自由函数 —— 纯 ImeView 构造器,与状态机无关)──────────────────────────

    /// Commit view with the caret at the end of the committed text.
pub(crate) fn commit_view(text: &str) -> ImeView {
        let mut v = ImeView::empty();
        ImeView::set_str(&mut v.commit_text, text);
        v.commit_cursor = ImeView::str_field(&v.commit_text).len() as u32;
        v.action = crate::frontend::action::COMMIT | crate::frontend::action::HANDLED;
        v
    }

    /// Commit with the application caret placed at `cursor` (byte offset into the
    /// committed text) — snippet templates with `$CURSOR` land here. Clamped to
    /// the actually-committed length (the buffer may truncate long text).
pub(crate) fn commit_view_at(text: &str, cursor: usize) -> ImeView {
        let mut v = ImeView::empty();
        ImeView::set_str(&mut v.commit_text, text);
        let len = ImeView::str_field(&v.commit_text).len();
        v.commit_cursor = cursor.min(len) as u32;
        v.action = crate::frontend::action::COMMIT | crate::frontend::action::HANDLED;
        v
    }

    /// View that passes the current key through to the application untouched.
pub(crate) fn passthrough_view() -> ImeView {
        let mut v = ImeView::empty();
        v.action = crate::frontend::action::PASSTHROUGH;
        v
    }

    /// 删除视图:不提交文本,只让前端删掉文本框中 `count` 个字符(`#del`)。
pub(crate) fn delete_view(count: u32) -> ImeView {
        tracing::debug!(count, "delete_view → ImeView.delete_count");
        let mut v = ImeView::empty();
        v.delete_count = count;
        v.action = crate::frontend::action::HANDLED;
        v
    }


/// 把 preedit 里的字面换行转义成 `\n` 文本(展示用),并同步调整光标字节偏移:
/// 每个**光标之前**的 `\n`(1 字节)→ `\n` 两字符(2 字节),光标后移 1 字节。
/// 提交/拼写等不含 `\n` 的场景原样返回,零开销。
pub(crate) fn escape_preedit(text: &str, cursor: usize) -> (String, usize) {
        if !text.contains('\n') {
            return (text.to_string(), cursor);
        }
        let mut escaped = String::with_capacity(text.len() + 4);
        let mut out_cursor = cursor;
        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                escaped.push_str("\\n");
                if i < cursor {
                    out_cursor += 1; // 光标前的 `\n` 扩成两字节,光标后移 1
                }
            } else {
                escaped.push(ch);
            }
        }
        (escaped, out_cursor)
}

// ── 全局调整 ────────────────────────────────────────────────────────────

/// Stage 3 置顶段(位置规则,先于过滤链):单字母输入的字母本尊 + 大小写
/// 互换置顶(english family 的 self/case 成员)。**它不是过滤器** —— 是
/// 位置调整,不进 Verdict 语义;与 merge / FilterChain / 造词区同为 stage3
/// 管线的固定工序,顺序:merge → promote → filters → 造词区。
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