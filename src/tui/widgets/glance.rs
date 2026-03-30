/// Glance pane — compact item status list between Workflows and Log panes.
///
/// Shows a list of issues/PRs with status badges.  Supports the same
/// item actions as the Info expanded panel (filter, open URL, clear errors).
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, LeftPane};
use crate::tui::events::ItemStatus;

/// Glance pane background.
const BG_GLANCE: Color = Color::Rgb(22, 22, 30);

/// Highlighted row background.
const BG_SELECTED: Color = Color::Rgb(36, 36, 48);

/// Active border color.
const BORDER_ACTIVE: Color = Color::Rgb(60, 80, 160);

/// Glance pane width in characters.
pub const GLANCE_WIDTH: u16 = 24;

/// Draw the Glance pane.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let bg = Style::default().bg(BG_GLANCE);
    let is_active = app.active_left_pane == LeftPane::Glance && !app.info_expanded_open;
    let border_style = if is_active {
        Style::default().fg(BORDER_ACTIVE).bg(BG_GLANCE)
    } else {
        bg
    };
    let border_char = if is_active { "│" } else { " " };

    let mut lines: Vec<Line> = Vec::new();
    let items = app.selected_items();
    let selected = app.glance_selected;

    // "All logs" row.
    let all_active = app.log_filter.is_none();
    let all_selected = selected == 0;
    let all_bg = if all_selected {
        Style::default().bg(BG_SELECTED)
    } else {
        bg
    };
    let indicator = if all_selected { ">" } else { " " };
    let marker = if all_active { "▶" } else { " " };

    lines.push(Line::from(vec![
        Span::styled(border_char, border_style),
        Span::styled(indicator, all_bg.fg(Color::White)),
        Span::styled(
            format!("{} All logs", marker),
            all_bg.fg(if all_active {
                Color::White
            } else {
                Color::Gray
            }),
        ),
    ]));

    // Items.
    let visible_height = area.height.saturating_sub(3) as usize; // -1 for All logs, -2 for footer
    if items.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(border_char, border_style),
            Span::styled(" ...", bg.fg(Color::DarkGray)),
        ]));
    } else {
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
            let panel_idx = idx + 1;
            let is_sel = panel_idx == selected;
            let is_filter_active = app
                .log_filter
                .as_ref()
                .map(|f| f == &item.id)
                .unwrap_or(false);
            let row_bg = if is_sel {
                Style::default().bg(BG_SELECTED)
            } else {
                bg
            };

            let (badge, badge_color) = status_badge(item.status);
            let ind = if is_sel { ">" } else { " " };
            let marker = if is_filter_active { "▶" } else { " " };

            // Truncate title to fit: width - border(1) - ind(1) - badge(4) - marker(1) - id(~8) - spaces(2)
            let id_display = &item.id;
            let max_title = (GLANCE_WIDTH as usize).saturating_sub(id_display.len() + 10);
            let title = if item.title.len() > max_title && max_title > 1 {
                format!("{}…", &item.title[..max_title.saturating_sub(1)])
            } else if max_title == 0 {
                String::new()
            } else {
                item.title.clone()
            };

            lines.push(Line::from(vec![
                Span::styled(border_char, border_style),
                Span::styled(ind, row_bg.fg(Color::White)),
                Span::styled(badge, row_bg.fg(badge_color)),
                Span::styled(marker, row_bg.fg(Color::White)),
                Span::styled(format!("{} ", id_display), row_bg.fg(Color::Cyan)),
                Span::styled(title, row_bg.fg(Color::DarkGray)),
            ]));
        }
    }

    // Pad to footer.
    let footer_row = area.height.saturating_sub(2) as usize;
    while lines.len() < footer_row {
        lines.push(Line::from(vec![
            Span::styled(border_char, border_style),
            Span::styled(" ", bg),
        ]));
    }

    // Footer hints (compact).
    lines.push(Line::from(vec![
        Span::styled(border_char, border_style),
        Span::styled(
            "─".repeat(GLANCE_WIDTH as usize - 2),
            bg.fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(border_char, border_style),
        Span::styled("⏎ ", bg.fg(Color::White)),
        Span::styled("g ", bg.fg(Color::White)),
        Span::styled("c ", bg.fg(Color::White)),
        Span::styled("ESC", bg.fg(Color::DarkGray)),
    ]));

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
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
