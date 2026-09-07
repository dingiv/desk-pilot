//! FreqMember — `#freq` 手工微调词条频率(round22 ④,round23 交互改版)。
//!
//! 链式用法(上游首选 = 分链时刻的面板高亮词,`fsm::family_prediction`
//! 的 chain_anchor;`ChainContext.root_text` 提供拼音绑定):
//!
//! ```text
//! women            → 正常预测页(women 高亮)
//! women'           → 页面不变(链式透明透传)
//! women'#f         → 补全提示(#freq / #freq/up / #freq/down + 回滚)
//! women'#freq      → 查询视图:1. women:[有效频率]±0  2…3. up/down 补全
//! women'#freq/up   → 步长菜单:±0 / +10 / +100 / +1000 / +10000(数字键选)
//! women'#freq/down → 步长菜单:±0 / −10 / −100 / −1000 / −10000
//!   选中步长 → 记账 → 结果视图「women ↑ 15000 → 16000」→ 空格提交该词
//! ```
//!
//! 步长是**量化菜单**而非一次性大步:细调(+10/+100)粗调(+1000/+10000)
//! 由用户指尖决定;±0 = 只看不改(选中后直接进结果视图,空格提交词)。

use super::member::{ChainContext, ContextKind, MagicMember, Prediction};
use super::FamilyEnv;

/// 步长档位(量化菜单;0 = 不改只查)。
pub const FREQ_STEPS: [i64; 5] = [0, 10, 100, 1_000, 10_000];

/// 菜单态:最近一次 `#freq/up|down` 预测的上下文(pick 时据此记账)。
struct MenuState {
    input: String,
    word: String,
    root: String,
    dir: i64,
    effective: u64,
}

/// 结果态:已记账,回显完整命令形态(`women'#freq/down/1000`)+ 变化量。
struct AppliedState {
    /// 触发本次记账的命令输入(菜单态 = "#freq/up";带参态 = "#freq/up/10000")。
    input: String,
    word: String,
    cmd: String,
    before: u64,
    after: u64,
}

pub struct FreqMember {
    menu: Option<MenuState>,
    applied: Option<AppliedState>,
}

impl FreqMember {
    pub fn new() -> Self {
        FreqMember { menu: None, applied: None }
    }

    /// 命令输入 → (方向, 步长参数)。方向:Some(1)=up / Some(-1)=down /
    /// None=查询;步长:完整命令形态 `#freq/down/1000` 的量级参数
    ///(无参 → None,走量化菜单)。
    fn parse(input: &str) -> (Option<i64>, Option<i64>) {
        let rest = input.trim_start_matches('#').strip_prefix("freq").unwrap_or("");
        let mut segs = rest.trim_start_matches('/').split('/');
        let dir = match segs.next() {
            Some("up") => Some(1),
            Some("down") => Some(-1),
            _ => return (None, None),
        };
        let mag = segs.next().and_then(|s| s.parse::<i64>().ok());
        (dir, mag)
    }

    /// 词条当前有效频率(±0 询问:step=0 不改账本;未入册词条此时顺手
    /// 建账,返回建账后的有效频率,避免首查显示 0)。
    fn effective_of(env: &dyn FamilyEnv, root: &str, word: &str) -> Option<u64> {
        env.adjust_word_freq(root, word, 0).map(|(_, after)| after)
    }

    /// 结果视图(单一出口:完整命令 · before → after,提交 raw = 词)。
    fn result_view(&self) -> Vec<Prediction> {
        let Some(a) = &self.applied else {
            return Vec::new();
        };
        vec![Prediction::commit_raw(
            format!("{} · {} → {}", a.cmd, a.before, a.after),
            a.word.clone(),
        )]
    }
}

impl Default for FreqMember {
    fn default() -> Self {
        Self::new()
    }
}

impl MagicMember for FreqMember {
    fn name(&self) -> &'static str {
        "freq"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__FREQ__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(FreqMember::new())
    }

    /// 完整触发路径:查询 + 两个方向。
    fn registered_paths(&self) -> Vec<String> {
        vec!["freq".into(), "freq/up".into(), "freq/down".into()]
    }

    /// 感知上游(First):高亮锚定的词条 + 链根拼音。
    fn wants_context(&self) -> Option<ContextKind> {
        Some(ContextKind::First)
    }

    fn predict_with_context(
        &mut self,
        _ctx: usize,
        input: &str,
        upstream: &ChainContext,
        env: &dyn FamilyEnv,
    ) -> Vec<Prediction> {
        let word = upstream.first_text().to_string();
        if word.is_empty() {
            return vec![Prediction::interactive("(上游无候选 — 用法:women'#freq/up)")];
        }
        let root = upstream.root_text.clone();

        // 结果视图(同词条已记账):回显**完整命令形态** + 变化量,
        // 空格提交该词(round23:选中档位后提示补全为 women'#freq/down/1000)。
        if let Some(a) = &self.applied {
            if a.word == word && a.input == input {
                return vec![Prediction::commit_raw(
                    format!("{} · {} → {}", a.cmd, a.before, a.after),
                    a.word.clone(),
                )];
            }
        }
        self.applied = None;

        let (dir_of, mag) = Self::parse(input);
        // 完整命令形态(#freq/down/1000):带参直接执行(幂等:同输入同词只记一次)。
        if let (Some(dir), Some(step)) = (dir_of, mag) {
            let Some(eff) = Self::effective_of(env, &root, &word) else {
                return vec![Prediction::interactive("(词频调整未接线)")];
            };
            let _ = eff;
            let (before, after) = env
                .adjust_word_freq(&root, &word, step * dir)
                .unwrap_or((0, 0));
            self.applied = Some(AppliedState {
                input: input.to_string(),
                word: word.clone(),
                cmd: format!("{root}'#freq/{}/{}", if dir > 0 { "up" } else { "down" }, step),
                before,
                after,
            });
            return self.result_view();
        }
        match dir_of {
            None => {
                // 查询视图:`#freq` = 当前有效频率(±0),up/down 由补全提示续入。
                match Self::effective_of(env, &root, &word) {
                    Some(eff) => vec![Prediction::interactive(format!("{word}:{eff}±0"))],
                    None => vec![Prediction::interactive("(词频查询未接线)")],
                }
            }
            Some(dir) => {
                // 步长菜单:量化档位,数字键选中 → pick 记账。
                let Some(eff) = Self::effective_of(env, &root, &word) else {
                    return vec![Prediction::interactive("(词频调整未接线)")];
                };
                self.menu = Some(MenuState {
                    input: input.to_string(),
                    word: word.clone(),
                    root,
                    dir,
                    effective: eff,
                });
                let sign = if dir > 0 { "+" } else { "−" };
                FREQ_STEPS
                    .iter()
                    .map(|s| {
                        if *s == 0 {
                            Prediction::interactive(format!("{word}:{eff}±0"))
                        } else {
                            Prediction::interactive(format!("{word}:{eff}{sign}{s}"))
                        }
                    })
                    .collect()
            }
        }
    }

    /// 数字键选中菜单档位:记账 → 结果视图(不上屏)。
    fn pick(&mut self, index: usize, _text: &str, _ctx: usize, env: &dyn FamilyEnv) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let Some(&step) = FREQ_STEPS.get(index) else {
            // 越界(菜单被翻页等):回菜单态。
            self.menu = Some(menu);
            return;
        };
        let delta = step * menu.dir;
        let (before, after) = env
            .adjust_word_freq(&menu.root, &menu.word, delta)
            .unwrap_or((menu.effective, menu.effective));
        self.applied = Some(AppliedState {
            input: menu.input,
            word: menu.word,
            cmd: format!(
                "{}'#freq/{}/{}",
                menu.root,
                if menu.dir > 0 { "up" } else { "down" },
                step
            ),
            before,
            after,
        });
    }

    /// 无上游的单独调用:提示链式用法(选中不上屏)。
    fn predict(&mut self, _ctx: usize, _input: &str, _env: &dyn FamilyEnv) -> Vec<Prediction> {
        vec![Prediction::interactive("用法:高亮目标词后 上游'#freq/up|down")]
    }

    fn tick(&mut self, _ctx: usize, _buffer: &str, _env: &dyn FamilyEnv) -> Option<Vec<Prediction>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths_and_magnitude_args() {
        assert_eq!(FreqMember::parse("#freq"), (None, None), "裸查询");
        assert_eq!(FreqMember::parse("#freq/up"), (Some(1), None), "方向菜单");
        assert_eq!(FreqMember::parse("#freq/down"), (Some(-1), None));
        assert_eq!(FreqMember::parse("#freq/down/1000"), (Some(-1), Some(1000)), "完整命令形态");
        assert_eq!(FreqMember::parse("#freq/up/abc"), (Some(1), None), "非数字参数回退菜单");
        assert_eq!(FreqMember::parse("#freq/sideways"), (None, None));
    }
}
