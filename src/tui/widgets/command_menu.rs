/// Command menu overlay — shown at the bottom when the user presses `:`.
///
/// Displays available single-key commands.  Dismisses on Esc or any action.
/// Styled as a floating bar with a subtle background, no borders.
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, InputMode};

/// Command menu background — slightly lighter to float above the log pane.
const BG_MENU: Color = Color::Rgb(36, 36, 48);

/// Draw the command menu overlay if the app is in Command mode.
pub fn draw(f: &mut Frame, app: &App) {
    if app.input_mode != InputMode::Command {
        return;
    }

    let area = f.area();

    // Place the menu at the bottom, 1 line tall.
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

    // Clear the area behind the overlay.
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
