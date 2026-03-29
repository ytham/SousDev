/// Command menu overlay — shown at the bottom when the user presses `:`.
/// Also renders the cron edit input when in CronEdit mode.
///
/// Displays available single-key commands.  Dismisses on Esc or any action.
/// Styled as a floating bar with a subtle background, no borders.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, InputMode};

/// Command menu background — slightly lighter to float above the log pane.
const BG_MENU: Color = Color::Rgb(36, 36, 48);

/// Draw the command menu or cron edit overlay.
pub fn draw(f: &mut Frame, app: &App) {
    match app.input_mode {
        InputMode::Command => draw_command_menu(f, app),
        InputMode::CronEdit => draw_cron_edit(f, app),
        _ => {}
    }
}

/// Draw the command menu overlay.
fn draw_command_menu(f: &mut Frame, _app: &App) {
    let area = f.area();
    let menu_height = 1u16;
    if area.height < menu_height + 1 {
        return;
    }

    let menu_area = Rect {
        x: area.x,
        y: area.y + area.height - menu_height,
        width: area.width,
        height: menu_height,
    };

    f.render_widget(Clear, menu_area);

    let bg = Style::default().bg(BG_MENU);
    let line = Line::from(vec![
        Span::styled(" ", bg),
        Span::styled(
            " ESC ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(180, 160, 60)),
        ),
        Span::styled(" close  ", bg.fg(Color::Gray)),
        Span::styled(
            " q ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(180, 60, 60)),
        ),
        Span::styled(" quit  ", bg.fg(Color::Gray)),
        Span::styled(
            " e ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(60, 160, 80)),
        ),
        Span::styled(" enable/disable  ", bg.fg(Color::Gray)),
        Span::styled(
            " c ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(180, 130, 60)),
        ),
        Span::styled(" schedule  ", bg.fg(Color::Gray)),
        Span::styled(
            " p ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(60, 140, 180)),
        ),
        Span::styled(" pause/resume ", bg.fg(Color::Gray)),
    ]);

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(line).block(block);
    f.render_widget(paragraph, menu_area);
}

/// Draw the cron schedule edit input overlay.
fn draw_cron_edit(f: &mut Frame, app: &App) {
    let area = f.area();
    let edit_height = 1u16;
    if area.height < edit_height + 1 {
        return;
    }

    let edit_area = Rect {
        x: area.x,
        y: area.y + area.height - edit_height,
        width: area.width,
        height: edit_height,
    };

    f.render_widget(Clear, edit_area);

    let bg = Style::default().bg(BG_MENU);
    let cursor = "\u{2588}"; // Block cursor character

    let line = Line::from(vec![
        Span::styled(" Schedule: ", bg.fg(Color::DarkGray)),
        Span::styled(
            app.cron_input.clone(),
            bg.fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(cursor, bg.fg(Color::Gray)),
        Span::styled("  (cron or e.g. 5m, 30min, 2hr) ", bg.fg(Color::DarkGray)),
        Span::styled(
            " ENTER ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(60, 160, 80)),
        ),
        Span::styled(" apply  ", bg.fg(Color::Gray)),
        Span::styled(
            " ESC ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(180, 160, 60)),
        ),
        Span::styled(" cancel ", bg.fg(Color::Gray)),
    ]);

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(line).block(block);
    f.render_widget(paragraph, edit_area);
}
