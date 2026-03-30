/// Sidebar widget — shows all workflows with compact vertical flowcharts.
///
/// The selected workflow is marked with a `>` indicator.  Each stage in the
/// flowchart is rendered with a status symbol:
///   `[ ]` pending, `[>]` running, `[+]` done, `[!]` failed.
///
/// Styled with a subtle background color, no borders.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, LeftPane, StageStatus, WorkflowStatus};
use crate::tui::ui::{ACCENT_BORDER, BG_SIDEBAR};

/// Draw the sidebar in the given area.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let bg = Style::default().bg(BG_SIDEBAR);
    let is_active = app.active_left_pane == LeftPane::Workflows && !app.info_expanded_open;
    let border_style = if is_active {
        Style::default().fg(ACCENT_BORDER).bg(BG_SIDEBAR)
    } else {
        bg
    };
    let border_char = if is_active { "▎ " } else { "  " };
    let mut lines: Vec<Line> = Vec::new();

    // Title line.
    lines.push(Line::from(Span::styled(
        " Workflows",
        Style::default()
            .fg(Color::DarkGray)
            .bg(BG_SIDEBAR)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for (i, wf) in app.workflows.iter().enumerate() {
        let is_selected = i == app.selected;

        // Workflow header line.
        let indicator = if is_selected { ">" } else { " " };
        let status_color = match wf.status {
            WorkflowStatus::Idle => Color::DarkGray,
            WorkflowStatus::Running => Color::Yellow,
            WorkflowStatus::Success => Color::Green,
            WorkflowStatus::Failed => Color::Red,
            WorkflowStatus::Skipped => Color::DarkGray,
            WorkflowStatus::Disabled => Color::DarkGray,
        };

        let name_style = if !wf.enabled {
            bg.fg(Color::DarkGray).add_modifier(Modifier::DIM)
        } else if is_selected {
            bg.fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            bg.fg(Color::Gray)
        };

        let enabled_tag = if wf.enabled { "" } else { " OFF" };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", indicator),
                if is_selected {
                    bg.fg(Color::White)
                } else {
                    bg.fg(status_color)
                },
            ),
            Span::styled(wf.name.clone(), name_style),
            Span::styled(enabled_tag, bg.fg(Color::DarkGray)),
        ]));

        // Human-readable schedule line.
        let schedule_text = describe_cron(&wf.schedule);
        lines.push(Line::from(vec![
            Span::styled("   ", bg),
            Span::styled(schedule_text, bg.fg(Color::DarkGray)),
        ]));

        // Compact flowchart for each stage.
        for (j, stage) in wf.stages.iter().enumerate() {
            let status = wf
                .stage_statuses
                .get(j)
                .copied()
                .unwrap_or(StageStatus::Pending);
            let (symbol, color) = match status {
                StageStatus::Pending => ("[ ]", Color::DarkGray),
                StageStatus::Running => ("[>]", Color::Yellow),
                StageStatus::Done => ("[+]", Color::Green),
                StageStatus::Failed => ("[!]", Color::Red),
            };

            let short_name = abbreviate_stage(stage);
            lines.push(Line::from(vec![
                Span::styled("   ", bg),
                Span::styled(symbol, bg.fg(color)),
                Span::styled(" ", bg),
                Span::styled(
                    short_name,
                    bg.fg(if status == StageStatus::Running {
                        Color::Yellow
                    } else {
                        Color::Gray
                    }),
                ),
            ]));
        }

        // Separator between workflows.
        if i + 1 < app.workflows.len() {
            lines.push(Line::from(""));
        }
    }

    if app.workflows.is_empty() {
        lines.push(Line::from(Span::styled(
            " No workflows configured",
            bg.fg(Color::DarkGray),
        )));
    }

    // Pad to fill the sidebar, then add key hints at the bottom.
    let hints_lines = 3;
    let hints_row = area.height.saturating_sub(hints_lines) as usize;
    while lines.len() < hints_row {
        lines.push(Line::from(Span::styled(" ", bg)));
    }
    lines.push(Line::from(vec![
        Span::styled(" ↑↓ ", bg.fg(Color::White)),
        Span::styled("select workflow", bg.fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" ←→ ", bg.fg(Color::White)),
        Span::styled("switch pane", bg.fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  i  ", bg.fg(Color::White)),
        Span::styled("info expanded", bg.fg(Color::DarkGray)),
    ]));

    // Prepend the active border character to every line.
    let lines: Vec<Line> = lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled(border_char, border_style)];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

/// Convert a 6-field cron expression to a short human-readable string.
///
/// Handles the common patterns used in SousDev configs.  The 6-field
/// format is `sec min hour day month weekday`.
fn describe_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 6 {
        return expr.to_string();
    }
    let (_sec, min, hour, day, month, dow) = (
        fields[0], fields[1], fields[2], fields[3], fields[4], fields[5],
    );

    // Every N minutes: "0 */N * * * *"
    if let Some(n) = min.strip_prefix("*/") {
        if hour == "*" && day == "*" && month == "*" && dow == "*" {
            return if n == "1" {
                "every minute".into()
            } else {
                format!("every {} min", n)
            };
        }
    }

    // Every N hours: "0 0 */N * * *"
    if let Some(n) = hour.strip_prefix("*/") {
        if min == "0" && day == "*" && month == "*" && dow == "*" {
            return if n == "1" {
                "every hour".into()
            } else {
                format!("every {} hours", n)
            };
        }
    }

    // Top of every hour: "0 0 * * * *"
    if min == "0" && hour == "*" && day == "*" && month == "*" && dow == "*" {
        return "every hour".into();
    }

    // Specific minute each hour: "0 30 * * * *"
    if hour == "*" && day == "*" && month == "*" && dow == "*" {
        if let Ok(m) = min.parse::<u32>() {
            return format!("hourly at :{:02}", m);
        }
    }

    // Daily at HH:MM: "0 M H * * *"
    if day == "*" && month == "*" && dow == "*" {
        if let (Ok(h), Ok(m)) = (hour.parse::<u32>(), min.parse::<u32>()) {
            return format!("daily at {:02}:{:02}", h, m);
        }
    }

    // Every N seconds: "*/N * * * * *"
    if let Some(n) = fields[0].strip_prefix("*/") {
        if min == "*" && hour == "*" && day == "*" && month == "*" && dow == "*" {
            return format!("every {} sec", n);
        }
    }

    // Every minute: "0 * * * * *"
    if fields[0] == "0" && min == "*" && hour == "*" && day == "*" && month == "*" && dow == "*" {
        return "every minute".into();
    }

    // Fallback: return the raw expression.
    expr.to_string()
}

/// Abbreviate a stage name to fit within the sidebar.
fn abbreviate_stage(name: &str) -> String {
    match name {
        "agent-loop" => "agent".into(),
        "review-feedback-loop" => "review".into(),
        "pr-description" => "pr-desc".into(),
        "pull-request" => "pr-push".into(),
        "pr-checkout" => "checkout".into(),
        "pr-review-poster" => "post".into(),
        "pr-comment-responder" => "respond".into(),
        other => {
            if other.len() > 16 {
                format!("{}...", &other[..13])
            } else {
                other.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── abbreviate_stage ──────────────────────────────────────────────────

    #[test]
    fn test_abbreviate_known_stages() {
        assert_eq!(abbreviate_stage("agent-loop"), "agent");
        assert_eq!(abbreviate_stage("review-feedback-loop"), "review");
        assert_eq!(abbreviate_stage("pr-description"), "pr-desc");
        assert_eq!(abbreviate_stage("pull-request"), "pr-push");
        assert_eq!(abbreviate_stage("pr-checkout"), "checkout");
    }

    #[test]
    fn test_abbreviate_long_unknown_stage() {
        assert_eq!(
            abbreviate_stage("a-very-long-stage-name"),
            "a-very-long-s..."
        );
    }

    #[test]
    fn test_abbreviate_short_unknown_stage() {
        assert_eq!(abbreviate_stage("foo"), "foo");
    }

    // ── describe_cron ─────────────────────────────────────────────────────

    #[test]
    fn test_every_n_minutes() {
        assert_eq!(describe_cron("0 */5 * * * *"), "every 5 min");
        assert_eq!(describe_cron("0 */15 * * * *"), "every 15 min");
        assert_eq!(describe_cron("0 */30 * * * *"), "every 30 min");
        assert_eq!(describe_cron("0 */1 * * * *"), "every minute");
    }

    #[test]
    fn test_every_hour() {
        assert_eq!(describe_cron("0 0 * * * *"), "every hour");
    }

    #[test]
    fn test_every_n_hours() {
        assert_eq!(describe_cron("0 0 */2 * * *"), "every 2 hours");
        assert_eq!(describe_cron("0 0 */6 * * *"), "every 6 hours");
        assert_eq!(describe_cron("0 0 */1 * * *"), "every hour");
    }

    #[test]
    fn test_hourly_at_specific_minute() {
        assert_eq!(describe_cron("0 30 * * * *"), "hourly at :30");
        assert_eq!(describe_cron("0 45 * * * *"), "hourly at :45");
        assert_eq!(describe_cron("0 5 * * * *"), "hourly at :05");
    }

    #[test]
    fn test_daily_at_time() {
        assert_eq!(describe_cron("0 0 9 * * *"), "daily at 09:00");
        assert_eq!(describe_cron("0 30 14 * * *"), "daily at 14:30");
        assert_eq!(describe_cron("0 0 0 * * *"), "daily at 00:00");
    }

    #[test]
    fn test_every_n_seconds() {
        assert_eq!(describe_cron("*/10 * * * * *"), "every 10 sec");
        assert_eq!(describe_cron("*/30 * * * * *"), "every 30 sec");
    }

    #[test]
    fn test_every_minute_star_form() {
        assert_eq!(describe_cron("0 * * * * *"), "every minute");
    }

    #[test]
    fn test_fallback_for_complex_expressions() {
        assert_eq!(describe_cron("0 0 9 1 * *"), "0 0 9 1 * *");
        assert_eq!(describe_cron("0 0 9 * * 1"), "0 0 9 * * 1");
        assert_eq!(describe_cron("0 0 9 * 6 *"), "0 0 9 * 6 *");
    }

    #[test]
    fn test_fallback_wrong_field_count() {
        assert_eq!(describe_cron("* * * * *"), "* * * * *");
        assert_eq!(describe_cron(""), "");
    }

    #[test]
    fn test_actual_config_schedules() {
        assert_eq!(describe_cron("0 0 * * * *"), "every hour");
        assert_eq!(describe_cron("0 */30 * * * *"), "every 30 min");
        assert_eq!(describe_cron("0 */15 * * * *"), "every 15 min");
    }
}
