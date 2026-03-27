/// Log view widget — scrollable log pane with a workflow header.
///
/// Shows the name and repo of the selected workflow at the top, then
/// streams log lines below.  Supports scroll via `log_scroll` offset
/// (0 = auto-tail to the bottom).
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::App;

/// Draw the log view in the given area.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(1),    // log content
        ])
        .split(area);

    draw_header(f, app, chunks[0]);
    draw_logs(f, app, chunks[1]);
}

/// Draw the header bar showing the selected workflow name and repo.
fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (title, repo) = match app.selected_workflow() {
        Some(wf) => (
            wf.name.clone(),
            wf.repo.clone().unwrap_or_else(|| "(no repo)".into()),
        ),
        None => ("(none)".into(), "".into()),
    };

    let status = app
        .selected_workflow()
        .map(|wf| format!("{}", wf.status))
        .unwrap_or_default();

    let status_color = match status.as_str() {
        "running" => Color::Yellow,
        "success" => Color::Green,
        "failed" => Color::Red,
        _ => Color::DarkGray,
    };

    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", title),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("| {} ", repo), Style::default().fg(Color::Cyan)),
        Span::styled(format!("| {} ", status), Style::default().fg(status_color)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let paragraph = Paragraph::new(line).block(block);
    f.render_widget(paragraph, area);
}

/// Draw the scrollable log content.
fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let wf = match app.selected_workflow() {
        Some(wf) => wf,
        None => {
            let empty = Paragraph::new(Span::styled(
                " Waiting for workflows...",
                Style::default().fg(Color::DarkGray),
            ));
            f.render_widget(empty, inner);
            return;
        }
    };

    if wf.logs.is_empty() {
        let empty = Paragraph::new(Span::styled(
            " Waiting for activity...",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(empty, inner);
        return;
    }

    let visible_height = inner.height as usize;
    let total = wf.logs.len();

    // log_scroll == 0 means auto-tail (show the last N lines).
    // log_scroll > 0 means scrolled up by that many lines from the bottom.
    let start = if app.log_scroll == 0 {
        total.saturating_sub(visible_height)
    } else {
        total
            .saturating_sub(visible_height)
            .saturating_sub(app.log_scroll)
    };
    let end = (start + visible_height).min(total);

    let lines: Vec<Line> = wf.logs[start..end]
        .iter()
        .map(|log| {
            let level_color = match log.level.as_str() {
                "error" => Color::Red,
                "warn" => Color::Yellow,
                "debug" => Color::DarkGray,
                _ => Color::Blue,
            };

            Line::from(vec![
                Span::styled(
                    format!("{:<5} ", log.level.to_uppercase()),
                    Style::default().fg(level_color),
                ),
                Span::styled(
                    format!("[{}] ", log.stage),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(&log.message),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(paragraph, inner);
}
