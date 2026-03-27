/// Command menu overlay — shown at the bottom when the user presses Esc.
///
/// Displays available single-key commands.  Dismisses on Esc or any action.
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::{App, InputMode};

/// Draw the command menu overlay if the app is in Command mode.
pub fn draw(f: &mut Frame, app: &App) {
    if app.input_mode != InputMode::Command {
        return;
    }

    let area = f.area();

    // Place the menu at the bottom, 3 lines tall.
    let menu_height = 3u16;
    if area.height < menu_height + 2 {
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

    let line = Line::from(vec![
        Span::styled(" ESC ", Style::default().fg(Color::Black).bg(Color::Yellow)),
        Span::raw(" close  "),
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Red)),
        Span::raw(" quit  "),
        Span::styled(" e ", Style::default().fg(Color::Black).bg(Color::Green)),
        Span::raw(" enable/disable  "),
        Span::styled(" p ", Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(" pause/resume  "),
    ]);

    let block = Block::default()
        .title(" Commands ")
        .title_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let paragraph = Paragraph::new(line).block(block);
    f.render_widget(paragraph, menu_area);
}
