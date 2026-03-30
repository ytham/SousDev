/// Command menu overlay — shown at the bottom when the user presses `:`.
/// Also renders the cron edit input when in CronEdit mode.
///
/// Styled to match the overall muted dark theme with a blue left border.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, InputMode};

use crate::tui::ui::{ACCENT_BORDER, BG_MENU};

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
    let border = Style::default().fg(ACCENT_BORDER).bg(BG_MENU);
    let key = bg.fg(Color::White);
    let label = bg.fg(Color::DarkGray);

    let line = Line::from(vec![
        Span::styled("│", border),
        Span::styled(" ESC ", key),
        Span::styled("close  ", label),
        Span::styled("q ", key),
        Span::styled("quit  ", label),
        Span::styled("e ", key),
        Span::styled("enable/disable  ", label),
        Span::styled("c ", key),
        Span::styled("schedule  ", label),
        Span::styled("p ", key),
        Span::styled("pause/resume", label),
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
    let border = Style::default().fg(ACCENT_BORDER).bg(BG_MENU);
    let key = bg.fg(Color::White);
    let label = bg.fg(Color::DarkGray);
    let cursor = "\u{2588}";

    let line = Line::from(vec![
        Span::styled("│", border),
        Span::styled(" Schedule: ", label),
        Span::styled(
            app.cron_input.clone(),
            bg.fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(cursor, bg.fg(Color::Gray)),
        Span::styled("  (cron or e.g. 5m, 30min, 2hr) ", label),
        Span::styled("ENTER ", key),
        Span::styled("apply  ", label),
        Span::styled("ESC ", key),
        Span::styled("cancel", label),
    ]);

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(line).block(block);
    f.render_widget(paragraph, edit_area);
}
