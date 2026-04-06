/// Info pane — compact item status list between Workflows and Log panes.
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
use crate::tui::ui::{ACCENT_BORDER, BG_INFO, BG_ROW_FOCUS};

/// Info pane width in characters.
pub const INFO_WIDTH: u16 = 34;

/// Draw the Glance pane.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let bg = Style::default().bg(BG_INFO);
    let is_active = app.active_left_pane == LeftPane::Info && !app.info_expanded_open;
    let border_style = if is_active {
        Style::default().fg(ACCENT_BORDER).bg(BG_INFO)
    } else {
        bg
    };
    let border_char = if is_active { "▎ " } else { "  " };

    let mut lines: Vec<Line> = Vec::new();
    let items = app.selected_items();
    let selected = app.info_selected;

    // Workflow name header.
    let wf_name = app.selected_workflow_name().unwrap_or("—");
    let max_name = (INFO_WIDTH as usize).saturating_sub(3);
    let name_display = if wf_name.len() > max_name {
        crate::utils::truncate::safe_truncate(wf_name, max_name)
    } else {
        wf_name.to_string()
    };
    let title_color = if is_active { Color::White } else { Color::Gray };
    lines.push(Line::from(vec![
        Span::styled(border_char, border_style),
        Span::styled(
            name_display,
            bg.fg(title_color)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(border_char, border_style),
        Span::styled(" ", bg),
    ]));

    // "All logs" row.
    let all_active = app.log_filter.is_none();
    let all_selected = selected == 0;
    let all_bg = if all_selected {
        Style::default().bg(BG_ROW_FOCUS)
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
    let visible_height = area.height.saturating_sub(8) as usize; // -1 header, -1 spacer, -1 All logs, -5 footer
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
                Style::default().bg(BG_ROW_FOCUS)
            } else {
                bg
            };

            let (badge, badge_color) = status_badge_for_item(item);
            let ind = if is_sel { ">" } else { " " };
            let marker = if is_filter_active { "▶" } else { " " };

            // Truncate title to fit: width - border(1) - ind(1) - badge(4) - marker(1) - id(~8) - spaces(2)
            let id_display = &item.id;
            let max_title = (INFO_WIDTH as usize).saturating_sub(id_display.len() + 10);
            let title = if item.title.len() > max_title && max_title > 1 {
                crate::utils::truncate::safe_truncate(&item.title, max_title)
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
                Span::styled(title, row_bg.fg(Color::Gray)),
            ]));
        }
    }

    // Pad to footer.
    let footer_lines = 5;
    let footer_row = area.height.saturating_sub(footer_lines) as usize;
    while lines.len() < footer_row {
        lines.push(Line::from(vec![
            Span::styled(border_char, border_style),
            Span::styled(" ", bg),
        ]));
    }

    // ── Selected item detail (2 lines above hints) ───────────────────
    let max_detail_width = (INFO_WIDTH as usize).saturating_sub(3); // border + space
    if selected > 0 && selected <= items.len() {
        let item = &items[selected - 1];

        // Line 1: Status in human-readable words.
        let description = status_description(item.status);
        let desc_truncated = if description.len() > max_detail_width {
            crate::utils::truncate::safe_truncate(description, max_detail_width)
        } else {
            description.to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(border_char, border_style),
            Span::styled(desc_truncated, bg.fg(Color::DarkGray)),
        ]));

        // Line 2: Issue/PR number (teal) + name.
        // Strip leading "#" or "PR #" to save space.
        let num_display = item.id.trim_start_matches("PR ").trim_start_matches('#');
        let name_budget = max_detail_width.saturating_sub(num_display.len() + 1);
        let name = if item.title.len() > name_budget && name_budget > 1 {
            crate::utils::truncate::safe_truncate(&item.title, name_budget)
        } else if name_budget == 0 {
            String::new()
        } else {
            item.title.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(border_char, border_style),
            Span::styled(format!("{} ", num_display), bg.fg(Color::Cyan)),
            Span::styled(name, bg.fg(Color::Gray)),
        ]));
    } else {
        // No item selected — show blank lines.
        for _ in 0..2 {
            lines.push(Line::from(vec![
                Span::styled(border_char, border_style),
                Span::styled(" ", bg),
            ]));
        }
    }

    // ── Footer hints ─────────────────────────────────────────────────
    lines.push(Line::from(vec![
        Span::styled(border_char, border_style),
        Span::styled("─".repeat(INFO_WIDTH as usize - 3), bg.fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(border_char, border_style),
        Span::styled("↑↓ ", bg.fg(Color::White)),
        Span::styled("select  ", bg.fg(Color::DarkGray)),
        Span::styled("⏎ ", bg.fg(Color::White)),
        Span::styled("show", bg.fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(border_char, border_style),
        Span::styled("c/C ", bg.fg(Color::White)),
        Span::styled("clear/all  ", bg.fg(Color::DarkGray)),
        Span::styled("g ", bg.fg(Color::White)),
        Span::styled("open", bg.fg(Color::DarkGray)),
    ]));

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

/// Return a short human-readable description of an item status.
fn status_description(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::None => "Not yet processed",
        ItemStatus::InProgress => "In progress",
        ItemStatus::Success => "PR opened / completed",
        ItemStatus::Error => "Failed (will retry after cooldown)",
        ItemStatus::Cooldown => "In cooldown (will retry later)",
        ItemStatus::ReviewedApproved => "Agent approved",
        ItemStatus::ReviewedConcerns => "Agent found concerns",
        ItemStatus::Approved => "PR approved -- ready to merge",
        ItemStatus::PlanPending => "Plan waiting for review",
        ItemStatus::NewComments => "Has new reviewer comments",
        ItemStatus::NoNewComments => "No new comments",
    }
}

/// Return the display badge and color for an item status.
fn status_badge(status: ItemStatus) -> (String, Color) {
    match status {
        ItemStatus::None => ("[  ]".into(), Color::DarkGray),
        ItemStatus::InProgress => ("[>>]".into(), Color::Yellow),
        ItemStatus::Success => ("[PR]".into(), Color::Green),
        ItemStatus::Error => ("[!!]".into(), Color::Red),
        ItemStatus::Cooldown => ("[!!]".into(), Color::Red),
        ItemStatus::ReviewedApproved => ("[A✓]".into(), Color::Green),
        ItemStatus::ReviewedConcerns => ("[A✗]".into(), Color::Rgb(220, 120, 80)),
        ItemStatus::Approved => ("[✓✓]".into(), Color::Green),
        ItemStatus::PlanPending => ("[Pl]".into(), Color::Cyan),
        ItemStatus::NewComments => ("[**]".into(), Color::Cyan),
        ItemStatus::NoNewComments => ("[--]".into(), Color::DarkGray),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_badge_labels() {
        assert_eq!(status_badge(ItemStatus::None).0, "[  ]");
        assert_eq!(status_badge(ItemStatus::InProgress).0, "[>>]");
        assert_eq!(status_badge(ItemStatus::Success).0, "[PR]");
        assert_eq!(status_badge(ItemStatus::Error).0, "[!!]");
        assert_eq!(status_badge(ItemStatus::Cooldown).0, "[!!]");
        assert_eq!(status_badge(ItemStatus::ReviewedApproved).0, "[A✓]");
        assert_eq!(status_badge(ItemStatus::ReviewedConcerns).0, "[A✗]");
        assert_eq!(status_badge(ItemStatus::Approved).0, "[✓✓]");
        assert_eq!(status_badge(ItemStatus::PlanPending).0, "[Pl]");
        assert_eq!(status_badge(ItemStatus::NewComments).0, "[**]");
    }

    #[test]
    fn test_status_badge_colors() {
        assert_eq!(status_badge(ItemStatus::None).1, Color::DarkGray);
        assert_eq!(status_badge(ItemStatus::InProgress).1, Color::Yellow);
        assert_eq!(status_badge(ItemStatus::Success).1, Color::Green);
        assert_eq!(status_badge(ItemStatus::Error).1, Color::Red);
        assert_eq!(status_badge(ItemStatus::ReviewedApproved).1, Color::Green);
        assert_eq!(
            status_badge(ItemStatus::ReviewedConcerns).1,
            Color::Rgb(220, 120, 80)
        );
        assert_eq!(status_badge(ItemStatus::Approved).1, Color::Green);
        assert_eq!(status_badge(ItemStatus::PlanPending).1, Color::Cyan);
        assert_eq!(status_badge(ItemStatus::NewComments).1, Color::Cyan);
    }
}

/// Status badge that uses item context for richer display.
fn status_badge_for_item(item: &crate::tui::events::ItemSummary) -> (String, Color) {
    match item.status {
        ItemStatus::NewComments | ItemStatus::NoNewComments if item.comment_count > 0 => {
            let count_str = if item.comment_count >= 100 {
                format!("{}", item.comment_count % 100)
            } else {
                format!("{}", item.comment_count)
            };
            let color = if item.status == ItemStatus::NewComments {
                Color::Cyan
            } else {
                Color::DarkGray
            };
            (format!("[{:>2}]", count_str), color)
        }
        other => status_badge(other),
    }
}
