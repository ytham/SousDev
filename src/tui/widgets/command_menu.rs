/// Command menu overlay — shown at the bottom when the user presses `:`.
/// Also renders the cron edit input when in CronEdit mode.
///
/// Floating modals use consistent BG_INFO_EXPANDED background with
/// blue left border and standard margins.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, InputMode};
use crate::tui::ui::{ACCENT_BORDER, BG_INFO_EXPANDED};

/// Draw the command menu or cron edit overlay.
pub fn draw(f: &mut Frame, app: &App) {
    match app.input_mode {
        InputMode::Command => draw_command_menu(f, app),
        InputMode::CronEdit => draw_cron_edit(f, app),
        _ => {}
    }
}

/// Draw the command menu overlay.
///
/// Floats 1 line from the bottom, 2 chars from the left.
/// 1 row + 1 char padding all around inside the box.
fn draw_command_menu(f: &mut Frame, _app: &App) {
    let area = f.area();
    let menu_height = 4u16; // top pad + title + commands + bottom pad
    let margin_left: u16 = 2;
    let margin_bottom: u16 = 1;
    if area.height < menu_height + margin_bottom + 1 || area.width < margin_left + 20 {
        return;
    }

    let menu_area = Rect {
        x: area.x + margin_left,
        y: area.y + area.height - menu_height - margin_bottom,
        width: area.width - margin_left,
        height: menu_height,
    };

    f.render_widget(Clear, menu_area);

    let bg = Style::default().bg(BG_INFO_EXPANDED);
    let border = Style::default().fg(ACCENT_BORDER).bg(BG_INFO_EXPANDED);
    let key = bg.fg(Color::White);
    let label = bg.fg(Color::DarkGray);
    let version = env!("CARGO_PKG_VERSION");

    let lines = vec![
        // Top padding row.
        Line::from(vec![Span::styled("▎ ", border), Span::styled(" ", bg)]),
        // Title row.
        Line::from(vec![
            Span::styled("▎ ", border),
            Span::styled(format!(" 🧑‍🍳 SousDev v{}", version), bg.fg(Color::Gray)),
        ]),
        // Commands row.
        Line::from(vec![
            Span::styled("▎ ", border),
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
        ]),
        // Bottom padding row.
        Line::from(vec![Span::styled("▎ ", border), Span::styled(" ", bg)]),
    ];

    let block = Block::default().style(bg);
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, menu_area);
}

/// Draw the cron schedule edit input overlay.
///
/// Same positioning as the command menu.
fn draw_cron_edit(f: &mut Frame, app: &App) {
    let area = f.area();
    let edit_height = 1u16;
    let margin_left: u16 = 2;
    let margin_bottom: u16 = 1;
    if area.height < edit_height + margin_bottom + 1 || area.width < margin_left + 20 {
        return;
    }

    let edit_area = Rect {
        x: area.x + margin_left,
        y: area.y + area.height - edit_height - margin_bottom,
        width: area.width - margin_left,
        height: edit_height,
    };

    f.render_widget(Clear, edit_area);

    let bg = Style::default().bg(BG_INFO_EXPANDED);
    let border = Style::default().fg(ACCENT_BORDER).bg(BG_INFO_EXPANDED);
    let key = bg.fg(Color::White);
    let label = bg.fg(Color::DarkGray);
    let cursor = "\u{2588}";

    let line = Line::from(vec![
        Span::styled("▎ ", border),
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
