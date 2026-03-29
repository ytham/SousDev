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
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, Panel};
use crate::tui::ui::{BG_HEADER, BG_LOGS};

/// Highlight background for selected log lines.
const BG_SELECTION: Color = Color::Rgb(50, 50, 70);

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
        Span::styled("menu  ", bg.fg(Color::DarkGray)),
        Span::styled("i ", bg.fg(Color::White)),
        Span::styled("info  ", bg.fg(Color::DarkGray)),
        Span::styled("↑↓ ", bg.fg(Color::White)),
        Span::styled("select  ", bg.fg(Color::DarkGray)),
        Span::styled("f/b ", bg.fg(Color::White)),
        Span::styled("page  ", bg.fg(Color::DarkGray)),
        Span::styled("F/B ", bg.fg(Color::White)),
        Span::styled("end/begin", bg.fg(Color::DarkGray)),
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

    // Determine selection range (screen rows).
    let sel_active = app.selection.active && app.selection.panel == Some(Panel::Logs);
    let sel_top = app.selection.start_row.min(app.selection.end_row);
    let sel_bot = app.selection.start_row.max(app.selection.end_row);

    // Maximum content width (excluding the 1-char left padding).
    let max_width = area.width.saturating_sub(1) as usize;

    let lines: Vec<Line> = wf.logs[start..end]
        .iter()
        .enumerate()
        .map(|(i, log)| {
            let screen_row = area.y + i as u16;
            let is_selected = sel_active && screen_row >= sel_top && screen_row <= sel_bot;
            let row_bg = if is_selected {
                Style::default().bg(BG_SELECTION)
            } else {
                bg
            };

            let level_color = match log.level.as_str() {
                "error" => Color::Red,
                "warn" => Color::Yellow,
                "debug" => Color::DarkGray,
                _ => Color::Blue,
            };

            // Build the raw content, then truncate to panel width.
            let prefix = format!("{:<5} [{}] ", log.level.to_uppercase(), log.stage);
            let available = max_width.saturating_sub(prefix.len());
            let msg = if log.message.len() > available {
                format!("{}…", &log.message[..available.saturating_sub(1)])
            } else {
                log.message.clone()
            };

            Line::from(vec![
                Span::styled(" ", row_bg),
                Span::styled(
                    format!("{:<5} ", log.level.to_uppercase()),
                    row_bg.fg(level_color),
                ),
                Span::styled(format!("[{}] ", log.stage), row_bg.fg(Color::DarkGray)),
                Span::styled(msg, row_bg.fg(Color::Gray)),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(lines).block(Block::default().style(bg));
    f.render_widget(paragraph, area);
}
