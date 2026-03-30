/// Log view widget — scrollable log pane with a workflow header.
///
/// In pretty mode, shows structured log entries with consolidation,
/// expand/collapse, and visual separators.  In flat mode, shows raw
/// log lines as before.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::tui::app::{total_entry_rows, App, LogEntryKind, Panel};
use crate::tui::ui::{BG_HEADER, BG_LOGS};

/// Highlight background for selected log lines.
const BG_SELECTION: Color = Color::Rgb(50, 50, 70);

/// Left border color for thinking blocks.
const THOUGHT_BORDER: Color = Color::Rgb(80, 160, 200);

/// Draw the status bar showing the selected workflow name, repo, status, and key hints.
pub fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
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

    let filter_label = match &app.log_filter {
        None => "All logs".to_string(),
        Some(id) => id.clone(),
    };

    let info_line = Line::from(vec![
        Span::styled(
            format!(" {} ", title),
            bg.fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {} ", repo), bg.fg(Color::Cyan)),
        Span::styled(format!(" {} ", status), bg.fg(status_color)),
        Span::styled(" │ ", bg.fg(Color::DarkGray)),
        Span::styled(
            filter_label,
            bg.fg(if app.log_filter.is_some() {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        ),
    ]);

    // Second line: key hints for the log pane.
    let hints_line = Line::from(vec![
        Span::styled(" : ", bg.fg(Color::White)),
        Span::styled("menu  ", bg.fg(Color::DarkGray)),
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
    f.render_widget(block, area);

    let wf = match app.selected_workflow() {
        Some(wf) => wf,
        None => {
            let empty = Paragraph::new(Span::styled(
                " Waiting for workflows...",
                Style::default().bg(BG_LOGS).fg(Color::DarkGray),
            ))
            .block(Block::default().style(bg));
            f.render_widget(empty, area);
            return;
        }
    };

    if app.pretty_logs && !wf.log_entries.is_empty() {
        draw_logs_pretty(f, app, wf, area);
    } else if !wf.logs.is_empty() {
        draw_logs_flat(f, app, wf, area);
    } else {
        let msg = if app.log_filter.is_some() {
            " No logs for this item yet"
        } else {
            " Waiting for activity..."
        };
        let empty = Paragraph::new(Span::styled(
            msg,
            Style::default().bg(BG_LOGS).fg(Color::DarkGray),
        ))
        .block(Block::default().style(bg));
        f.render_widget(empty, area);
    }
}

// ---------------------------------------------------------------------------
// Pretty mode rendering
// ---------------------------------------------------------------------------

fn draw_logs_pretty(f: &mut Frame, app: &App, wf: &crate::tui::app::WorkflowState, area: Rect) {
    let bg = Style::default().bg(BG_LOGS);
    let visible_height = area.height as usize;
    let max_width = area.width.saturating_sub(2) as usize;

    let entries = &wf.log_entries;
    let total_rows = total_entry_rows(entries);

    // Determine the row offset for scrolling.
    let start_offset = if app.log_scroll == 0 {
        total_rows.saturating_sub(visible_height)
    } else {
        total_rows
            .saturating_sub(visible_height)
            .saturating_sub(app.log_scroll)
    };

    // Selection range.
    let sel_active = app.selection.active && app.selection.panel == Some(Panel::Logs);
    let sel_top = app.selection.start_row.min(app.selection.end_row);
    let sel_bot = app.selection.start_row.max(app.selection.end_row);

    let ctx = RenderCtx {
        bg,
        max_width,
        area,
        sel_active,
        sel_top,
        sel_bot,
    };

    // Render entries into screen lines.
    let mut lines: Vec<Line> = Vec::new();
    let mut row_accum = 0usize;

    for (i, entry) in entries.iter().enumerate() {
        let entry_rows = entry.row_count();
        let entry_end = row_accum + entry_rows;

        // Only render entries that overlap the visible window.
        if entry_end > start_offset && row_accum < start_offset + visible_height {
            let screen_y_base = area.y as usize + row_accum.saturating_sub(start_offset);

            match entry.kind {
                LogEntryKind::Thought => render_thought(entry, &mut lines, &ctx, screen_y_base),
                LogEntryKind::ToolCall => render_tool_call(entry, &mut lines, &ctx, screen_y_base),
                LogEntryKind::ConsolidatedTools => {
                    render_consolidated(entry, &mut lines, &ctx, screen_y_base)
                }
                LogEntryKind::System => render_system(entry, &mut lines, &ctx, screen_y_base),
            }
        }

        row_accum += entry_rows;

        // Blank separator between different kinds.
        if i + 1 < entries.len() && entries[i].kind != entries[i + 1].kind {
            if row_accum >= start_offset && row_accum < start_offset + visible_height {
                lines.push(Line::from(Span::styled(" ", bg)));
            }
            row_accum += 1;
        }
    }

    // Pad remaining lines.
    while lines.len() < visible_height {
        lines.push(Line::from(Span::styled(" ", bg)));
    }

    // Truncate to visible height (in case of rounding).
    lines.truncate(visible_height);

    let paragraph = Paragraph::new(lines).block(Block::default().style(bg));
    f.render_widget(paragraph, area);
}

/// Rendering context passed to each entry renderer to avoid too many arguments.
struct RenderCtx {
    bg: Style,
    max_width: usize,
    area: Rect,
    sel_active: bool,
    sel_top: u16,
    sel_bot: u16,
}

impl RenderCtx {
    fn row_style(&self, screen_row: usize) -> Style {
        let row = screen_row as u16;
        if self.sel_active && row >= self.sel_top && row <= self.sel_bot {
            Style::default().bg(BG_SELECTION)
        } else {
            self.bg
        }
    }
}

fn truncate_msg(msg: &str, max: usize) -> String {
    if msg.len() > max {
        format!("{}…", &msg[..max.saturating_sub(1)])
    } else {
        msg.to_string()
    }
}

fn render_thought(
    entry: &crate::tui::app::LogEntry,
    lines: &mut Vec<Line>,
    ctx: &RenderCtx,
    screen_y_base: usize,
) {
    let show_lines = if entry.expanded || entry.lines.len() <= 4 {
        entry.lines.len()
    } else {
        4
    };

    for (j, log) in entry.lines.iter().take(show_lines).enumerate() {
        let screen_row = screen_y_base + j;
        if screen_row >= ctx.area.y as usize + ctx.area.height as usize {
            break;
        }
        let rs = ctx.row_style(screen_row);
        let msg = truncate_msg(&log.message, ctx.max_width.saturating_sub(2));
        lines.push(Line::from(vec![
            Span::styled(" │", rs.fg(THOUGHT_BORDER)),
            Span::styled(format!(" {}", msg), rs.fg(Color::White)),
        ]));
    }

    if !entry.expanded && entry.lines.len() > 4 {
        let remaining = entry.lines.len() - 4;
        let screen_row = screen_y_base + 4;
        if screen_row < ctx.area.y as usize + ctx.area.height as usize {
            let rs = ctx.row_style(screen_row);
            lines.push(Line::from(vec![
                Span::styled(" │", rs.fg(THOUGHT_BORDER)),
                Span::styled(
                    format!(" […] {} more lines — click to expand", remaining),
                    rs.fg(Color::DarkGray),
                ),
            ]));
        }
    }
}

fn render_tool_call(
    entry: &crate::tui::app::LogEntry,
    lines: &mut Vec<Line>,
    ctx: &RenderCtx,
    screen_y_base: usize,
) {
    if let Some(tool_line) = entry.lines.first() {
        let rs = ctx.row_style(screen_y_base);
        let msg = truncate_msg(&tool_line.message, ctx.max_width.saturating_sub(8));
        lines.push(Line::from(vec![
            Span::styled(" ", rs),
            Span::styled("[tool] ", rs.fg(Color::Rgb(140, 120, 200))),
            Span::styled(msg, rs.fg(Color::Gray)),
        ]));
    }

    if entry.expanded {
        for (j, log) in entry.lines.iter().skip(1).enumerate() {
            let screen_row = screen_y_base + 1 + j;
            if screen_row >= ctx.area.y as usize + ctx.area.height as usize {
                break;
            }
            let rs = ctx.row_style(screen_row);
            let msg = truncate_msg(&log.message, ctx.max_width.saturating_sub(8));
            lines.push(Line::from(vec![
                Span::styled("   └─ ", rs.fg(Color::DarkGray)),
                Span::styled(msg, rs.fg(Color::DarkGray)),
            ]));
        }
    }
}

fn render_consolidated(
    entry: &crate::tui::app::LogEntry,
    lines: &mut Vec<Line>,
    ctx: &RenderCtx,
    screen_y_base: usize,
) {
    let tool_lines: Vec<&crate::tui::app::LogLine> =
        entry.lines.iter().filter(|l| l.stage == "tool").collect();

    if entry.expanded {
        for (j, tool_line) in tool_lines.iter().enumerate() {
            let screen_row = screen_y_base + j;
            if screen_row >= ctx.area.y as usize + ctx.area.height as usize {
                break;
            }
            let rs = ctx.row_style(screen_row);
            let msg = truncate_msg(&tool_line.message, ctx.max_width.saturating_sub(8));
            lines.push(Line::from(vec![
                Span::styled(" ", rs),
                Span::styled("[tool] ", rs.fg(Color::Rgb(140, 120, 200))),
                Span::styled(msg, rs.fg(Color::Gray)),
            ]));
        }
    } else {
        if let Some(last_tool) = tool_lines.last() {
            let rs = ctx.row_style(screen_y_base);
            let msg = truncate_msg(&last_tool.message, ctx.max_width.saturating_sub(8));
            lines.push(Line::from(vec![
                Span::styled(" ", rs),
                Span::styled("[tool] ", rs.fg(Color::Rgb(140, 120, 200))),
                Span::styled(msg, rs.fg(Color::Gray)),
            ]));
        }

        let count = tool_lines.len().saturating_sub(1);
        if count > 0 {
            let screen_row = screen_y_base + 1;
            if screen_row < ctx.area.y as usize + ctx.area.height as usize {
                let rs = ctx.row_style(screen_row);
                lines.push(Line::from(vec![
                    Span::styled(" ", rs),
                    Span::styled(
                        format!(
                            "[+] {} more tool call{} — click to expand",
                            count,
                            if count == 1 { "" } else { "s" }
                        ),
                        rs.fg(Color::DarkGray),
                    ),
                ]));
            }
        }
    }
}

fn render_system(
    entry: &crate::tui::app::LogEntry,
    lines: &mut Vec<Line>,
    ctx: &RenderCtx,
    screen_y_base: usize,
) {
    if let Some(log) = entry.lines.first() {
        let rs = ctx.row_style(screen_y_base);
        let level_color = match log.level.as_str() {
            "error" => Color::Red,
            "warn" => Color::Yellow,
            "debug" => Color::DarkGray,
            _ => Color::Blue,
        };
        let prefix = format!("{:<5} [{}] ", log.level.to_uppercase(), log.stage);
        let available = ctx.max_width.saturating_sub(prefix.len());
        let msg = truncate_msg(&log.message, available);

        lines.push(Line::from(vec![
            Span::styled(" ", rs),
            Span::styled(
                format!("{:<5} ", log.level.to_uppercase()),
                rs.fg(level_color),
            ),
            Span::styled(format!("[{}] ", log.stage), rs.fg(Color::DarkGray)),
            Span::styled(msg, rs.fg(Color::Gray)),
        ]));
    }
}

// ---------------------------------------------------------------------------
// Flat mode rendering (pretty = false)
// ---------------------------------------------------------------------------

fn draw_logs_flat(f: &mut Frame, app: &App, wf: &crate::tui::app::WorkflowState, area: Rect) {
    let bg = Style::default().bg(BG_LOGS);
    let visible_height = area.height as usize;
    let total = wf.logs.len();
    let max_width = area.width.saturating_sub(1) as usize;

    let start = if app.log_scroll == 0 {
        total.saturating_sub(visible_height)
    } else {
        total
            .saturating_sub(visible_height)
            .saturating_sub(app.log_scroll)
    };
    let end = (start + visible_height).min(total);

    let sel_active = app.selection.active && app.selection.panel == Some(Panel::Logs);
    let sel_top = app.selection.start_row.min(app.selection.end_row);
    let sel_bot = app.selection.start_row.max(app.selection.end_row);

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
