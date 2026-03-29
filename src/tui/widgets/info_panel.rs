/// Info panel widget — floating overlay showing issue/PR status per workflow.
///
/// Renders on the right side of the terminal, floating over the log pane.
/// Shows a list of items with status indicators.  The selected item is
/// highlighted and can be opened, cleared, etc. via keyboard shortcuts.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::events::ItemStatus;

/// Info panel background.
const BG_INFO: Color = Color::Rgb(26, 26, 36);

/// Highlighted row background.
const BG_SELECTED: Color = Color::Rgb(36, 36, 48);

/// Info panel width in characters.
pub const INFO_PANEL_WIDTH: u16 = 60;

/// Number of reserved rows at the top (title + separator).
const HEADER_ROWS: u16 = 2;

/// Number of reserved rows at the bottom (separator + hints).
const FOOTER_ROWS: u16 = 2;

/// Draw the info panel as a floating overlay if it is open.
pub fn draw(f: &mut Frame, app: &App) {
    if !app.info_panel_open {
        return;
    }

    let area = f.area();
    if area.width < INFO_PANEL_WIDTH + 28 {
        return;
    }

    let panel_area = Rect {
        x: area.x + area.width - INFO_PANEL_WIDTH,
        y: area.y,
        width: INFO_PANEL_WIDTH,
        height: area.height,
    };

    f.render_widget(Clear, panel_area);

    let bg = Style::default().bg(BG_INFO);
    let mut lines: Vec<Line> = Vec::new();

    // ── Header ────────────────────────────────────────────────────────────
    let wf_name = app
        .selected_workflow()
        .map(|wf| wf.name.as_str())
        .unwrap_or("(none)");
    lines.push(Line::from(Span::styled(
        format!(" {}", wf_name),
        bg.fg(Color::White).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        " ─".to_string() + &"─".repeat(INFO_PANEL_WIDTH as usize - 3),
        bg.fg(Color::DarkGray),
    )));

    // ── Items ─────────────────────────────────────────────────────────────
    let items = app.selected_items();
    let visible_height = panel_area.height.saturating_sub(HEADER_ROWS + FOOTER_ROWS) as usize;

    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            " Waiting for data...",
            bg.fg(Color::DarkGray),
        )));
    } else {
        // Scroll window: keep the selected item visible.
        let selected = app.info_panel_selected;
        let scroll_start = if selected >= visible_height {
            selected - visible_height + 1
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
            let is_selected = idx == selected;
            let row_bg = if is_selected {
                Style::default().bg(BG_SELECTED)
            } else {
                bg
            };

            let (badge, badge_color) = status_badge(item.status);
            let max_title = (INFO_PANEL_WIDTH as usize).saturating_sub(item.id.len() + 10);
            let title = if item.title.len() > max_title {
                format!("{}…", &item.title[..max_title.saturating_sub(1)])
            } else {
                item.title.clone()
            };

            let indicator = if is_selected { ">" } else { " " };

            lines.push(Line::from(vec![
                Span::styled(indicator, row_bg.fg(Color::White)),
                Span::styled(badge, row_bg.fg(badge_color)),
                Span::styled(" ", row_bg),
                Span::styled(format!("{:<8} ", item.id), row_bg.fg(Color::Cyan)),
                Span::styled(title, row_bg.fg(Color::Gray)),
            ]));
        }
    }

    // Pad between items and footer.
    let target_rows = panel_area.height.saturating_sub(FOOTER_ROWS) as usize;
    while lines.len() < target_rows {
        lines.push(Line::from(Span::styled(" ", bg)));
    }

    // ── Footer (hints) ────────────────────────────────────────────────────
    lines.push(Line::from(Span::styled(
        " ─".to_string() + &"─".repeat(INFO_PANEL_WIDTH as usize - 3),
        bg.fg(Color::DarkGray),
    )));
    lines.push(Line::from(vec![
        Span::styled(" ↑↓ ", bg.fg(Color::White)),
        Span::styled("select  ", bg.fg(Color::DarkGray)),
        Span::styled("⏎ ", bg.fg(Color::White)),
        Span::styled("open  ", bg.fg(Color::DarkGray)),
        Span::styled("c ", bg.fg(Color::White)),
        Span::styled("clear  ", bg.fg(Color::DarkGray)),
        Span::styled("C ", bg.fg(Color::White)),
        Span::styled("clear errors  ", bg.fg(Color::DarkGray)),
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
