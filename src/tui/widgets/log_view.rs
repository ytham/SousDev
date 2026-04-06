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

use crate::tui::app::{App, LogEntryKind, Panel};
use crate::tui::ui::{
    ACCENT_INFO_LEVEL, ACCENT_THOUGHT, ACCENT_TOOL, BG_LOGS, BG_STATUS_BAR, BG_TEXT_SELECTION,
    BG_THOUGHT,
};

/// Color palette for distinguishing concurrent items (PRs, issues) in logs.
/// 10 visually distinct colors that avoid clashing with the existing theme.
const ITEM_COLORS: [Color; 10] = [
    Color::Rgb(220, 160, 60),  // warm amber
    Color::Rgb(100, 200, 130), // soft mint
    Color::Rgb(200, 100, 160), // rose
    Color::Rgb(100, 180, 220), // sky blue
    Color::Rgb(180, 140, 100), // warm tan
    Color::Rgb(160, 200, 80),  // lime
    Color::Rgb(200, 130, 100), // salmon
    Color::Rgb(130, 160, 220), // periwinkle
    Color::Rgb(220, 120, 200), // orchid
    Color::Rgb(120, 200, 190), // teal
];

/// Map an item label to a consistent color from the palette.
fn item_color(label: &str) -> Color {
    let hash = label
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    ITEM_COLORS[(hash as usize) % ITEM_COLORS.len()]
}

/// Draw the status bar showing the selected workflow name, repo, status, and key hints.
pub fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let bg = Style::default().bg(BG_STATUS_BAR);

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

    let (filter_label, filter_title) = match &app.log_filter {
        None => ("All logs".to_string(), String::new()),
        Some(id) => {
            let item_title = app
                .selected_items()
                .iter()
                .find(|item| &item.id == id)
                .map(|item| item.title.clone())
                .unwrap_or_default();
            (id.clone(), item_title)
        }
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
        if !filter_title.is_empty() {
            Span::styled(format!(" {} ", filter_title), bg.fg(Color::DarkGray))
        } else {
            Span::raw("")
        },
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
    let total_rows = wf.log_entries_total_rows;

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

        // Early exit: if we've passed the visible window, stop rendering.
        if row_accum >= start_offset + visible_height {
            break;
        }

        // Only render entries that overlap the visible window.
        if entry_end > start_offset {
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
            Style::default().bg(BG_TEXT_SELECTION)
        } else {
            self.bg
        }
    }
}

/// Flatten newlines to double spaces for single-line display.
fn flatten_newlines(msg: &str) -> String {
    msg.replace('\n', "  ").replace('\r', "")
}

fn truncate_msg(msg: &str, max: usize) -> String {
    crate::utils::truncate::safe_truncate(msg, max)
}

/// Resolve the border/accent color for a log entry.  If the entry has an
/// item label, use the per-item color; otherwise use the default accent.
fn entry_accent(entry: &crate::tui::app::LogEntry, default: Color) -> Color {
    entry
        .lines
        .first()
        .and_then(|l| l.item_label.as_deref())
        .map(item_color)
        .unwrap_or(default)
}

fn render_thought(
    entry: &crate::tui::app::LogEntry,
    lines: &mut Vec<Line>,
    ctx: &RenderCtx,
    screen_y_base: usize,
) {
    let accent = entry_accent(entry, ACCENT_THOUGHT);

    // Thought-specific style: subtle background + colored left border.
    let thought_style = |screen_row: usize| -> Style {
        let base = ctx.row_style(screen_row);
        // Use thought background unless the row is text-selected.
        if base.bg == Some(BG_TEXT_SELECTION) {
            base
        } else {
            Style::default().bg(BG_THOUGHT)
        }
    };

    // Count total visual lines (LogLines + embedded newlines).
    let total_visual: usize = entry
        .lines
        .iter()
        .map(|l| l.message.split('\n').count())
        .sum();

    if total_visual <= 1 {
        // Truly single-line thought — show fully, not expandable.
        if let Some(log) = entry.lines.first() {
            let ts = thought_style(screen_y_base);
            let msg = truncate_msg(
                &flatten_newlines(&log.message),
                ctx.max_width.saturating_sub(4),
            );
            lines.push(Line::from(vec![
                Span::styled(" ▎", ts.fg(accent)),
                Span::styled(format!(" {}", msg), ts.fg(Color::White)),
            ]));
        }
    } else if entry.expanded {
        // Expanded — show all lines, splitting on embedded newlines.
        let mut row_offset = 0;
        for log in &entry.lines {
            for sub_line in log.message.split('\n') {
                let screen_row = screen_y_base + row_offset;
                if screen_row >= ctx.area.y as usize + ctx.area.height as usize {
                    break;
                }
                let ts = thought_style(screen_row);
                let msg = truncate_msg(sub_line, ctx.max_width.saturating_sub(4));
                lines.push(Line::from(vec![
                    Span::styled(" ▎", ts.fg(accent)),
                    Span::styled(format!(" {}", msg), ts.fg(Color::White)),
                ]));
                row_offset += 1;
            }
        }
    } else {
        // Collapsed — show first line (newlines flattened) + expand indicator.
        if let Some(log) = entry.lines.first() {
            let ts = thought_style(screen_y_base);
            let msg = truncate_msg(
                &flatten_newlines(&log.message),
                ctx.max_width.saturating_sub(4),
            );
            lines.push(Line::from(vec![
                Span::styled(" ▎", ts.fg(accent)),
                Span::styled(format!(" {}", msg), ts.fg(Color::White)),
            ]));
        }
        let remaining = total_visual - 1;
        let screen_row = screen_y_base + 1;
        if screen_row < ctx.area.y as usize + ctx.area.height as usize {
            let ts = thought_style(screen_row);
            lines.push(Line::from(vec![
                Span::styled(" ▎", ts.fg(accent)),
                Span::styled(
                    format!(
                        " […] {} more line{}",
                        remaining,
                        if remaining == 1 { "" } else { "s" }
                    ),
                    ts.fg(Color::DarkGray),
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
    let accent = entry_accent(entry, ACCENT_TOOL);

    if let Some(tool_line) = entry.lines.first() {
        let rs = ctx.row_style(screen_y_base);
        let msg = truncate_msg(&tool_line.message, ctx.max_width.saturating_sub(8));
        lines.push(Line::from(vec![
            Span::styled(" ", rs),
            Span::styled("[tool] ", rs.fg(accent)),
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
    let accent = entry_accent(entry, ACCENT_TOOL);
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
                Span::styled("[tool] ", rs.fg(accent)),
                Span::styled(msg, rs.fg(Color::Gray)),
            ]));
        }
    } else {
        if let Some(last_tool) = tool_lines.last() {
            let rs = ctx.row_style(screen_y_base);
            let msg = truncate_msg(&last_tool.message, ctx.max_width.saturating_sub(8));
            lines.push(Line::from(vec![
                Span::styled(" ", rs),
                Span::styled("[tool] ", rs.fg(accent)),
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
            _ => ACCENT_INFO_LEVEL,
        };

        // Colored dot prefix for item-labeled system entries.
        let dot = if let Some(ref label) = log.item_label {
            Span::styled("● ", rs.fg(item_color(label)))
        } else {
            Span::styled("  ", rs)
        };

        let prefix_len = 2 + 6 + log.stage.len() + 3; // dot + level + [stage] + spaces
        let available = ctx.max_width.saturating_sub(prefix_len);

        if entry.expanded {
            // Expanded: show the full message, wrapping on newlines.
            // First line: level + stage + first line of message.
            let msg_lines: Vec<&str> = log.message.split('\n').collect();
            let first_msg = if !msg_lines.is_empty() {
                msg_lines[0]
            } else {
                &log.message
            };

            lines.push(Line::from(vec![
                dot.clone(),
                Span::styled(
                    format!("{:<5} ", log.level.to_uppercase()),
                    rs.fg(level_color),
                ),
                Span::styled(format!("[{}] ", log.stage), rs.fg(Color::DarkGray)),
                Span::styled(first_msg.to_string(), rs.fg(Color::Gray)),
            ]));

            // Remaining lines: indented continuation.
            for (j, sub_line) in msg_lines.iter().enumerate().skip(1) {
                let screen_row = screen_y_base + j;
                if screen_row >= ctx.area.y as usize + ctx.area.height as usize {
                    break;
                }
                let row_s = ctx.row_style(screen_row);
                let indent = " ".repeat(prefix_len);
                lines.push(Line::from(vec![
                    Span::styled(indent, row_s),
                    Span::styled(sub_line.to_string(), row_s.fg(Color::Gray)),
                ]));
            }

            // If the message was long but didn't have newlines, show it
            // fully without truncation on a single line (already done above
            // since we don't call truncate_msg).
        } else {
            // Collapsed: truncated to available width.
            let msg = truncate_msg(&flatten_newlines(&log.message), available);

            // Show expand indicator if the message is truncated.
            let is_expandable = log.message.len() > available || log.message.contains('\n');
            let suffix = if is_expandable { " ▸" } else { "" };

            lines.push(Line::from(vec![
                dot,
                Span::styled(
                    format!("{:<5} ", log.level.to_uppercase()),
                    rs.fg(level_color),
                ),
                Span::styled(format!("[{}] ", log.stage), rs.fg(Color::DarkGray)),
                Span::styled(msg, rs.fg(Color::Gray)),
                Span::styled(suffix.to_string(), rs.fg(Color::DarkGray)),
            ]));
        }
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
                Style::default().bg(BG_TEXT_SELECTION)
            } else {
                bg
            };

            let level_color = match log.level.as_str() {
                "error" => Color::Red,
                "warn" => Color::Yellow,
                "debug" => Color::DarkGray,
                _ => ACCENT_INFO_LEVEL,
            };

            // Colored dot prefix for item-labeled log lines.
            let dot = if let Some(ref label) = log.item_label {
                Span::styled("● ", row_bg.fg(item_color(label)))
            } else {
                Span::styled("  ", row_bg)
            };

            let prefix_len = 2 + 6 + log.stage.len() + 3; // dot + level + [stage] + spaces
            let available = max_width.saturating_sub(prefix_len);
            let msg = truncate_msg(&log.message, available);

            Line::from(vec![
                dot,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flatten_newlines_basic() {
        assert_eq!(flatten_newlines("a\nb\nc"), "a  b  c");
    }

    #[test]
    fn test_flatten_newlines_no_newlines() {
        assert_eq!(flatten_newlines("no newlines here"), "no newlines here");
    }

    #[test]
    fn test_flatten_newlines_cr_lf() {
        assert_eq!(flatten_newlines("line1\r\nline2"), "line1  line2");
    }

    #[test]
    fn test_flatten_newlines_empty() {
        assert_eq!(flatten_newlines(""), "");
    }

    #[test]
    fn test_truncate_msg_short() {
        assert_eq!(truncate_msg("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_msg_long() {
        assert_eq!(truncate_msg("hello world!", 8), "hello…");
    }

    #[test]
    fn test_truncate_msg_exact() {
        assert_eq!(truncate_msg("12345", 5), "12345");
    }
}
