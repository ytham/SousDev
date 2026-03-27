/// Top-level draw function — splits the terminal into sidebar + log pane.
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::widgets::{command_menu, log_view, sidebar};

/// Draw the full TUI layout.
pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(24), // sidebar
            Constraint::Min(40),    // log pane
        ])
        .split(f.area());

    sidebar::draw(f, app, chunks[0]);
    log_view::draw(f, app, chunks[1]);
    command_menu::draw(f, app);
}
