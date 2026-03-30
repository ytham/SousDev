/// Info expanded panel widget — floating overlay showing issue/PR status per workflow.
///
/// Renders on the right side of the terminal, floating over the log pane.
/// Shows "All logs" as the first selectable item, followed by a list of
/// items with status indicators.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::events::ItemStatus;

use crate::tui::ui::{ACCENT_BORDER, BG_INFO_EXPANDED, BG_ROW_FOCUS};

/// Info expanded panel width in characters.
pub const INFO_EXPANDED_WIDTH: u16 = 60;

/// Number of reserved rows at the top (title + separator).
const HEADER_ROWS: u16 = 2;

/// Number of reserved rows at the bottom (separator + hints).
const FOOTER_ROWS: u16 = 2;

/// Draw the info expanded panel as a floating overlay if it is open.
pub fn draw(f: &mut Frame, app: &App) {
    if !app.info_expanded_open {
        return;
    }

    let area = f.area();
    if area.width < INFO_EXPANDED_WIDTH + 20 {
        return;
    }

    // Position: left side with margins (10 from left, 2 from top/bottom).
    let margin_x: u16 = 10;
    let margin_y: u16 = 2;
    let panel_height = area.height.saturating_sub(margin_y * 2);
    if panel_height < 8 {
        return;
    }
    let panel_area = Rect {
        x: area.x + margin_x,
        y: area.y + margin_y,
        width: INFO_EXPANDED_WIDTH,
        height: panel_height,
    };

    f.render_widget(Clear, panel_area);

    let bg = Style::default().bg(BG_INFO_EXPANDED);
    let border = Style::default().fg(ACCENT_BORDER).bg(BG_INFO_EXPANDED);
    let mut lines: Vec<Line> = Vec::new();

    // ── Header ────────────────────────────────────────────────────────────
    let wf_name = app
        .selected_workflow()
        .map(|wf| wf.name.as_str())
        .unwrap_or("(none)");
    lines.push(Line::from(vec![
        Span::styled("│", border),
        Span::styled(
            format!(" {}", wf_name),
            bg.fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│", border),
        Span::styled(
            "─".repeat(INFO_EXPANDED_WIDTH as usize - 2),
            bg.fg(Color::DarkGray),
        ),
    ]));

    // ── "All logs" row ────────────────────────────────────────────────────
    let items = app.selected_items();
    let selected = app.info_expanded_selected;

    let all_logs_selected = selected == 0;
    let all_logs_active = app.log_filter.is_none();
    let all_logs_bg = if all_logs_selected {
        Style::default().bg(BG_ROW_FOCUS)
    } else {
        bg
    };
    let active_marker = if all_logs_active { "▶" } else { " " };
    let indicator = if all_logs_selected { ">" } else { " " };

    lines.push(Line::from(vec![
        Span::styled("│", border),
        Span::styled(indicator, all_logs_bg.fg(Color::White)),
        Span::styled(
            format!("{} All logs", active_marker),
            all_logs_bg.fg(if all_logs_active {
                Color::White
            } else {
                Color::Gray
            }),
        ),
    ]));

    // ── Items ─────────────────────────────────────────────────────────────
    let visible_height = panel_area
        .height
        .saturating_sub(HEADER_ROWS + FOOTER_ROWS + 1) as usize; // +1 for "All logs"

    if items.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("│", border),
            Span::styled(" Waiting for data...", bg.fg(Color::DarkGray)),
        ]));
    } else {
        // Scroll window: keep the selected item visible.
        // selected 0 = "All logs", 1+ = items. Item index = selected - 1.
        let item_selected = selected.saturating_sub(1);
        let scroll_start = if item_selected >= visible_height {
            item_selected - visible_height + 1
        } else {
            0
        };
        let scroll_end = (scroll_start + visible_height).min(items.len());

        for (idx, item) in items
            .iter()
            .enumerate()
            .skip(scroll_start)
            .take(scroll_end - scroll_start)
        {
            let panel_idx = idx + 1; // offset for "All logs"
            let is_selected = panel_idx == selected;
            let is_active = app
                .log_filter
                .as_ref()
                .map(|f| f == &item.id)
                .unwrap_or(false);
            let row_bg = if is_selected {
                Style::default().bg(BG_ROW_FOCUS)
            } else {
                bg
            };

            let (badge, badge_color) = status_badge(item.status);
            let max_title = (INFO_EXPANDED_WIDTH as usize).saturating_sub(item.id.len() + 12);
            let title = if item.title.len() > max_title {
                format!("{}…", &item.title[..max_title.saturating_sub(1)])
            } else {
                item.title.clone()
            };

            let indicator = if is_selected { ">" } else { " " };
            let active_marker = if is_active { "▶" } else { " " };

            lines.push(Line::from(vec![
                Span::styled("│", border),
                Span::styled(indicator, row_bg.fg(Color::White)),
                Span::styled(badge, row_bg.fg(badge_color)),
                Span::styled(active_marker, row_bg.fg(Color::White)),
                Span::styled(format!("{:<8} ", item.id), row_bg.fg(Color::Cyan)),
                Span::styled(title, row_bg.fg(Color::Gray)),
            ]));
        }
    }

    // Pad between items and footer.
    let target_rows = panel_area.height.saturating_sub(FOOTER_ROWS) as usize;
    while lines.len() < target_rows {
        lines.push(Line::from(vec![
            Span::styled("│", border),
            Span::styled(" ", bg),
        ]));
    }

    // ── Footer (hints) ────────────────────────────────────────────────────
    lines.push(Line::from(vec![
        Span::styled("│", border),
        Span::styled(
            "─".repeat(INFO_EXPANDED_WIDTH as usize - 2),
            bg.fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│", border),
        Span::styled(" ↑↓ ", bg.fg(Color::White)),
        Span::styled("select ", bg.fg(Color::DarkGray)),
        Span::styled("⏎ ", bg.fg(Color::White)),
        Span::styled("filter ", bg.fg(Color::DarkGray)),
        Span::styled("g ", bg.fg(Color::White)),
        Span::styled("open ", bg.fg(Color::DarkGray)),
        Span::styled("c ", bg.fg(Color::White)),
        Span::styled("clear ", bg.fg(Color::DarkGray)),
        Span::styled("ESC ", bg.fg(Color::White)),
        Span::styled("close", bg.fg(Color::DarkGray)),
    ]));

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, panel_area);
}

/// Return the display badge and color for an item status.
fn status_badge(status: ItemStatus) -> (String, Color) {
    match status {
        ItemStatus::None => ("[  ]".into(), Color::DarkGray),
        ItemStatus::InProgress => ("[>>]".into(), Color::Yellow),
        ItemStatus::Success => ("[PR]".into(), Color::Green),
        ItemStatus::Error => ("[!!]".into(), Color::Red),
        ItemStatus::Cooldown => ("[!!]".into(), Color::Red),
        ItemStatus::Reviewed => ("[OK]".into(), Color::Green),
        ItemStatus::NewComments => ("[**]".into(), Color::Cyan),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_badge_colors() {
        assert_eq!(status_badge(ItemStatus::None).0, "[  ]");
        assert_eq!(status_badge(ItemStatus::InProgress).0, "[>>]");
        assert_eq!(status_badge(ItemStatus::Success).0, "[PR]");
        assert_eq!(status_badge(ItemStatus::Error).0, "[!!]");
        assert_eq!(status_badge(ItemStatus::Reviewed).0, "[OK]");
        assert_eq!(status_badge(ItemStatus::NewComments).0, "[**]");
    }
}
