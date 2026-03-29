/// Info panel widget — floating overlay showing issue/PR status per workflow.
///
/// Renders on the right side of the terminal, floating over the log pane.
/// Shows a list of items with status indicators.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::events::ItemStatus;

/// Info panel background.
const BG_INFO: Color = Color::Rgb(26, 26, 36);

/// Info panel width in characters.
pub const INFO_PANEL_WIDTH: u16 = 40;

/// Draw the info panel as a floating overlay if it is open.
pub fn draw(f: &mut Frame, app: &App) {
    if !app.info_panel_open {
        return;
    }

    let area = f.area();
    if area.width < INFO_PANEL_WIDTH + 28 {
        return; // Terminal too narrow — skip the panel.
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

    // Title.
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

    let items = app.selected_items();

    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            " Waiting for data...",
            bg.fg(Color::DarkGray),
        )));
    } else {
        for item in items {
            let (badge, badge_color) = status_badge(item.status);
            // Truncate title to fit: panel_width - badge(4) - id(~10) - padding(4)
            let max_title = (INFO_PANEL_WIDTH as usize).saturating_sub(item.id.len() + 10);
            let title = if item.title.len() > max_title {
                format!("{}…", &item.title[..max_title.saturating_sub(1)])
            } else {
                item.title.clone()
            };

            lines.push(Line::from(vec![
                Span::styled(" ", bg),
                Span::styled(badge, bg.fg(badge_color)),
                Span::styled(" ", bg),
                Span::styled(format!("{:<8} ", item.id), bg.fg(Color::Cyan)),
                Span::styled(title, bg.fg(Color::Gray)),
            ]));
        }
    }

    // Pad remaining lines with background.
    while lines.len() < panel_area.height as usize {
        lines.push(Line::from(Span::styled(" ", bg)));
    }

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
