//! TUI frontend — passes keys directly to the engine and renders ImeView.
//!
//! 不做任何键拦截:crossterm 事件(含 Ctrl/Alt/Shift 修饰状态)忠实转成
//! [`KeyEvent`](ime_core::router::KeyEvent) 喂给引擎的输入路由层,再按
//! `ImeView::action` 反应 —— `COMMIT` 追加历史,PASSTHROUGH 的组合键
//! (Ctrl+Q/Ctrl+C)退出。

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::ExecutableCommand;
use ime_core::asr_buffer::AsrBuffer;
use ime_core::engine::ImeEngine;
use ime_core::router::{KeyKind, KeyEvent};
use ime_core::ImeView;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use swift_ime::bridge::{AuraConn, AuraConnHandle};
use swift_ime::frontends::mock::MockConfig;

const POLL_MS: u64 = 100; // voice live-refresh cadence (was 200)

// ── Entry point ────────────────────────────────────────────────────────

pub fn run(cfg: MockConfig) -> io::Result<()> {
    let (mut engine, asr_buffer, aura_status) = swift_ime::frontends::mock::build_engine(&cfg);

    let mut history: Vec<String> = Vec::new();

    enable_raw_mode()?;
    io::stdout().execute(crossterm::terminal::EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let res = run_loop(&mut terminal, &mut engine, &asr_buffer, &aura_status, &mut history);

    disable_raw_mode()?;
    io::stdout().execute(crossterm::terminal::LeaveAlternateScreen)?;
    res
}

// ── Main loop ──────────────────────────────────────────────────────────

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    engine: &mut ImeEngine,
    asr_buffer: &AsrBuffer,
    aura_status: &Option<AuraConnHandle>,
    history: &mut Vec<String>,
) -> io::Result<()> {
    let mut last_view = ImeView::empty();
    let mut should_quit = false;

    while !should_quit {
        terminal.draw(|f| {
            // 当前预测项的提供者(family/source)与权重(score)。
            let detailed = engine.candidates_detailed();
            let flags = engine.state_flags();
            render(f, &last_view, history, asr_buffer, aura_status, &detailed, flags)
        })?;

        if event::poll(Duration::from_millis(POLL_MS))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }

                // ── 忠实转换:键 + 修饰状态 → 统一键事件,交给路由层 ──
                let ev = crossterm_to_key(&key);

                // TUI 自身是"应用":引擎对 Ctrl 组合返回 PASSTHROUGH,
                // Ctrl+Q / Ctrl+C 在此退出。
                if ev.ctrl && matches!(ev.kind, KeyKind::Char('q') | KeyKind::Char('c')) {
                    should_quit = true;
                    continue;
                }

                last_view = engine.key(ev);
                let committed = ImeView::str_field(&last_view.commit_text);
                if !committed.is_empty() {
                    history.push(committed.to_string());
                }
            }
        }

        // ── Async poll ──
        let (code, view) = engine.poll_async();
        if code == 1 {
            last_view = view;
        } else if code == 2 {
            let committed = ImeView::str_field(&view.commit_text);
            if !committed.is_empty() {
                history.push(committed.to_string());
            }
            last_view = view;
        }

        // ── Magic live-refresh ── while a live magic command is active (`#asr`
        // voice anchor, `#req` HTTP request, …), rebuild the candidate view when
        // the member's async state advanced, without a keypress.
        if let Some(v) = engine.magic_tick() {
            last_view = v;
        }
    }

    Ok(())
}

// ── Key conversion ──────────────────────────────────────────────────────

/// crossterm 事件 → 统一键事件(忠实转换:键类 + Ctrl/Shift/Alt 状态)。
/// 字符经 [`KeyEvent::char`] 归一化(空格/数字/翻页符号各自成类)。
fn crossterm_to_key(key: &crossterm::event::KeyEvent) -> KeyEvent {
    let kind = match key.code {
        KeyCode::Char(c) => KeyEvent::char(c).kind,
        KeyCode::Enter => KeyKind::Enter,
        KeyCode::Backspace => KeyKind::Backspace,
        KeyCode::Esc => KeyKind::Escape,
        KeyCode::Tab => KeyKind::Tab,
        KeyCode::Up => KeyKind::Up,
        KeyCode::Down => KeyKind::Down,
        KeyCode::Left => KeyKind::Left,
        KeyCode::Right => KeyKind::Right,
        KeyCode::PageUp => KeyKind::PageUp,
        KeyCode::PageDown => KeyKind::PageDown,
        KeyCode::Home => KeyKind::Home,
        KeyCode::End => KeyKind::End,
        KeyCode::Delete => KeyKind::Delete,
        KeyCode::Insert => KeyKind::Insert,
        KeyCode::F(n) => KeyKind::Function(n),
        _ => KeyKind::Other(0),
    };
    let m = key.modifiers;
    KeyEvent {
        kind,
        ctrl: m.contains(KeyModifiers::CONTROL),
        shift: m.contains(KeyModifiers::SHIFT),
        alt: m.contains(KeyModifiers::ALT),
    }
}

// ── Render ─────────────────────────────────────────────────────────────

fn render(
    f: &mut Frame,
    view: &ImeView,
    history: &[String],
    asr_buffer: &AsrBuffer,
    aura_status: &Option<AuraConnHandle>,
    detailed: &[ime_core::family::RankedCandidate],
    flags: ime_core::router::StateFlags,
) {
    let area = f.area();

    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(10),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);

    render_preedit(f, rows[0], view);
    render_candidates(f, rows[1], view, detailed);
    render_history(f, rows[2], history);
    render_status(f, rows[3], asr_buffer, aura_status, flags);
}

fn render_preedit(f: &mut Frame, area: Rect, view: &ImeView) {
    // 严格区分:aux_up = 原始输入(你打了什么);preedit_text = 合成结果(将提交)。
    let raw = ImeView::str_field(&view.aux_up);
    let result = ImeView::str_field(&view.preedit_text);
    let mut spans: Vec<Span> = Vec::new();
    if !raw.is_empty() {
        spans.push(Span::styled(format!("输入: {raw} "), Style::new().fg(Color::DarkGray)));
    }
    if !result.is_empty() && result != raw {
        spans.push(Span::styled(format!("→ 提交: {result}"), Style::new().fg(Color::Cyan)));
    }
    let text = if spans.is_empty() { " ".into() } else { Line::from(spans) };
    let p = Paragraph::new(text)
        .block(Block::new().borders(Borders::ALL).title("Input"));
    f.render_widget(p, area);
}

fn render_candidates(
    f: &mut Frame,
    area: Rect,
    view: &ImeView,
    detailed: &[ime_core::family::RankedCandidate],
) {
    let mut lines: Vec<Line> = Vec::new();
    let page = view.candidate_page as usize;
    let page_size = view.candidate_page_size.max(1) as usize;
    let start = page * page_size;
    let end = (start + page_size).min(view.candidate_count as usize);

    for i in start..end {
        let label = format!("{}.", (i % page_size) + 1);
        let text = ImeView::str_field(&view.candidates[i].text);
        let label_text = ImeView::str_field(&view.candidates[i].label);
        let is_hl = i == view.candidate_highlight as usize;

        let style = if is_hl {
            Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else if text.ends_with("...") {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(Color::White)
        };

        let prefix = if label_text.is_empty() { String::new() } else { format!("{label_text} ") };
        // 提供者(family/source)与权重(score) —— 灰色小字,便于调试候选来源。
        let detail = detailed.iter().find(|d| d.text == text);
        let mut spans = vec![
            Span::styled(format!("{label} "), Style::new().fg(Color::DarkGray)),
            Span::styled(format!("{prefix}{text}"), style),
        ];
        if let Some(d) = detail {
            spans.push(Span::styled(
                format!("  [{:.3} {}/{}]", d.score, d.family, d.source),
                Style::new().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled("(no candidates)", Style::new().fg(Color::DarkGray))));
    }

    let total = if view.candidate_count > 0 {
        (view.candidate_count as usize).div_ceil(page_size)
    } else { 1 };
    let title = format!("Candidates (page {}/{total})", page + 1);

    let p = Paragraph::new(lines).block(Block::new().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
}

fn render_history(f: &mut Frame, area: Rect, history: &[String]) {
    let text = history.join("");
    let p = Paragraph::new(text)
        .block(Block::new().borders(Borders::ALL).title("Committed"))
        .style(Style::new().fg(Color::Gray));
    f.render_widget(p, area);
}

fn render_status(
    f: &mut Frame,
    area: Rect,
    asr_buffer: &AsrBuffer,
    aura_status: &Option<AuraConnHandle>,
    flags: ime_core::router::StateFlags,
) {
    let voice = asr_buffer.snapshot();
    let vs = if voice.is_empty() { "ASR: idle".into() } else { format!("ASR: {}", &voice[..voice.len().min(30)]) };
    let aura = match aura_status {
        Some(h) => match h.get() {
            AuraConn::Connected => Span::styled(" aura:✓ ", Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)),
            AuraConn::Disconnected => Span::styled(" aura:✗ ", Style::new().fg(Color::Red)),
            AuraConn::Connecting => Span::styled(" aura:… ", Style::new().fg(Color::Yellow)),
        },
        None => Span::styled(" aura:off ", Style::new().fg(Color::DarkGray)),
    };
    // 输入路由层的状态机表 —— 当前处于哪些输入状态。
    let flags_str = if flags.labels().is_empty() {
        "IDLE".to_string()
    } else {
        flags.labels().join("|")
    };
    let line = Line::from(vec![
        Span::styled(" Ctrl+Q:quit ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Esc:cancel ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Space:commit ", Style::new().fg(Color::Green)),
        Span::styled(" ↑↓←→:nav ", Style::new().fg(Color::DarkGray)),
        Span::styled(" PgUp/Dn:page ", Style::new().fg(Color::DarkGray)),
        Span::styled(" 1-9:select ", Style::new().fg(Color::DarkGray)),
        Span::styled(format!(" [{flags_str}]"), Style::new().fg(Color::Blue)),
        aura,
        Span::styled(format!(" | {vs}"), Style::new().fg(Color::Gray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
