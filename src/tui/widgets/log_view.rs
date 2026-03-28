/// Log view widget — scrollable log pane with a workflow header.
///
/// Shows the name and repo of the selected workflow at the top, then
/// streams log lines below.  Supports scroll via `log_scroll` offset
/// (0 = auto-tail to the bottom).
///
/// Styled with subtle background colors, no borders.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::ui::{BG_HEADER, BG_LOGS};

/// Draw the info bar showing the selected workflow name, repo, status, and key hints.
pub fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let bg = Style::default().bg(BG_HEADER);

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

    // First line: workflow info.
    let info_line = Line::from(vec![
        Span::styled(
            format!(" {} ", title),
            bg.fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {} ", repo), bg.fg(Color::Cyan)),
        Span::styled(format!(" {} ", status), bg.fg(status_color)),
    ]);

    // Second line: key hints.
    let hints_line = Line::from(vec![
        Span::styled(" : ", bg.fg(Color::White)),
        Span::styled("commands  ", bg.fg(Color::DarkGray)),
        Span::styled("↑↓ ", bg.fg(Color::White)),
        Span::styled("select  ", bg.fg(Color::DarkGray)),
        Span::styled("pgup/pgdn ", bg.fg(Color::White)),
        Span::styled("scroll", bg.fg(Color::DarkGray)),
    ]);

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(vec![info_line, hints_line]).block(block);
    f.render_widget(paragraph, area);
}

/// Draw the scrollable log content.
pub fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    let bg = Style::default().bg(BG_LOGS);
    let block = Block::default().style(bg);
    let inner = block.inner(area);
    f.render_widget(block.clone(), area);

    let wf = match app.selected_workflow() {
        Some(wf) => wf,
        None => {
            let empty = Paragraph::new(Span::styled(
                " Waiting for workflows...",
                bg.fg(Color::DarkGray),
            ))
            .block(Block::default().style(bg));
            f.render_widget(empty, inner);
            return;
        }
    };

    if wf.logs.is_empty() {
        let empty = Paragraph::new(Span::styled(
            " Waiting for activity...",
            bg.fg(Color::DarkGray),
        ))
        .block(Block::default().style(bg));
        f.render_widget(empty, inner);
        return;
    }

    let visible_height = area.height as usize;
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
                Span::styled(" ", bg),
                Span::styled(
                    format!("{:<5} ", log.level.to_uppercase()),
                    bg.fg(level_color),
                ),
                Span::styled(format!("[{}] ", log.stage), bg.fg(Color::DarkGray)),
                Span::styled(log.message.clone(), bg.fg(Color::Gray)),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().style(bg));
    f.render_widget(paragraph, area);
}
