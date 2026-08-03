//! TUI frontend — passes keys directly to the engine and renders ImeView.
//!
//! No navigation logic here — Space, Enter, Escape, Backspace, digits
//! are all handled by the engine's built-in special key layer.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::ExecutableCommand;
use ime_core::asr_buffer::AsrBuffer;
use ime_core::engine::ImeEngine;
use ime_core::special_key::SpecialKey;
use ime_core::ImeView;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use swift_ime::frontends::mock::MockConfig;

const POLL_MS: u64 = 200;

// ── Entry point ────────────────────────────────────────────────────────

pub fn run(cfg: MockConfig) -> io::Result<()> {
    let (mut engine, asr_buffer) = swift_ime::frontends::mock::build_engine(&cfg);

    let mut history: Vec<String> = Vec::new();

    enable_raw_mode()?;
    io::stdout().execute(crossterm::terminal::EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let res = run_loop(&mut terminal, &mut engine, &asr_buffer, &mut history);

    disable_raw_mode()?;
    io::stdout().execute(crossterm::terminal::LeaveAlternateScreen)?;
    res
}

// ── Main loop ──────────────────────────────────────────────────────────

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    engine: &mut ImeEngine,
    asr_buffer: &AsrBuffer,
    history: &mut Vec<String>,
) -> io::Result<()> {
    let mut last_view = ImeView::empty();
    let mut should_quit = false;

    while !should_quit {
        terminal.draw(|f| render(f, &last_view, history, asr_buffer))?;

        if event::poll(Duration::from_millis(POLL_MS))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }

                // ── Non-character special keys → engine.special_key_ctx ──
                let sk = match key.code {
                    KeyCode::Esc     => { should_quit = true; continue; }
                    KeyCode::Up      => SpecialKey::Up,
                    KeyCode::Down    => SpecialKey::Down,
                    KeyCode::Left    => SpecialKey::Left,
                    KeyCode::Right   => SpecialKey::Right,
                    KeyCode::Tab     => SpecialKey::Tab,
                    KeyCode::PageUp  => SpecialKey::PageUp,
                    KeyCode::PageDown=> SpecialKey::PageDown,
                    KeyCode::Backspace => SpecialKey::Backspace,
                    KeyCode::Enter   => SpecialKey::Enter,
                    _ => {
                        // ── Character keys → engine.predict_ctx ──
                        // Space, digits, letters all go through here.
                        // The engine's special key layer intercepts Space/Enter/BS/Esc/1-9.
                        if let KeyCode::Char(c) = key.code {
                            last_view = engine.predict_ctx(0, c);
                            let committed = ImeView::str_field(&last_view.commit_text);
                            if !committed.is_empty() {
                                history.push(committed.to_string());
                            }
                        }
                        continue;
                    }
                };

                // Non-character special key (arrows, page, tab).
                last_view = engine.special_key_ctx(0, sk);
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
    }

    Ok(())
}

// ── Render ─────────────────────────────────────────────────────────────

fn render(f: &mut Frame, view: &ImeView, history: &[String], asr_buffer: &AsrBuffer) {
    let area = f.area();

    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(10),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);

    render_preedit(f, rows[0], view);
    render_candidates(f, rows[1], view);
    render_history(f, rows[2], history);
    render_status(f, rows[3], view, asr_buffer);
}

fn render_preedit(f: &mut Frame, area: Rect, view: &ImeView) {
    let preedit = ImeView::str_field(&view.preedit_text);
    let p = Paragraph::new(if preedit.is_empty() { " ".into() } else { preedit.to_string() })
        .block(Block::new().borders(Borders::ALL).title("Input"));
    f.render_widget(p, area);
}

fn render_candidates(f: &mut Frame, area: Rect, view: &ImeView) {
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
        lines.push(Line::from(vec![
            Span::styled(format!("{label} "), Style::new().fg(Color::DarkGray)),
            Span::styled(format!("{prefix}{text}"), style),
        ]));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled("(no candidates)", Style::new().fg(Color::DarkGray))));
    }

    let total = if view.candidate_count > 0 {
        (view.candidate_count as usize + page_size - 1) / page_size
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

fn render_status(f: &mut Frame, _area: Rect, view: &ImeView, asr_buffer: &AsrBuffer) {
    let voice = asr_buffer.snapshot();
    let vs = if voice.is_empty() { "ASR: idle".into() } else { format!("ASR: {}", &voice[..voice.len().min(30)]) };
    let line = Line::from(vec![
        Span::styled(" ESC:quit ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Space:commit ", Style::new().fg(Color::Green)),
        Span::styled(" ↑↓←→:nav ", Style::new().fg(Color::DarkGray)),
        Span::styled(" Tab:next ", Style::new().fg(Color::DarkGray)),
        Span::styled(" PgUp/Dn:page ", Style::new().fg(Color::DarkGray)),
        Span::styled(" 1-9:select ", Style::new().fg(Color::DarkGray)),
        Span::styled(format!(" | {vs}"), Style::new().fg(Color::Gray)),
    ]);
    let _ = view;
    f.render_widget(Paragraph::new(line), _area);
}
