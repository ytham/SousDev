/// Top-level draw function — splits the terminal into sidebar + log pane.
///
/// Layout uses subtle background color differences with 1-char gaps
/// between panels.  Panels extend to the terminal edges.
/// The info bar sits at the bottom of the main pane.
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::widgets::{command_menu, log_view, sidebar};

/// Panel background colors — subtle dark shades to differentiate areas.
pub const BG_SIDEBAR: Color = Color::Rgb(24, 24, 32);
pub const BG_HEADER: Color = Color::Rgb(28, 28, 38);
pub const BG_LOGS: Color = Color::Rgb(20, 20, 28);
/// The terminal default background shows through the 1-char gaps.
pub const BG_GAP: Color = Color::Reset;

/// Draw the full TUI layout.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Horizontal split: sidebar | 1-char gap | main pane.
    let sidebar_width: u16 = 26;
    let gap: u16 = 1;

    if area.width < sidebar_width + gap + 10 {
        // Terminal too narrow — just show sidebar.
        sidebar::draw(f, app, area);
        return;
    }

    let sidebar_area = Rect {
        x: area.x,
        y: area.y,
        width: sidebar_width,
        height: area.height,
    };

    let main_area = Rect {
        x: area.x + sidebar_width + gap,
        y: area.y,
        width: area.width - sidebar_width - gap,
        height: area.height,
    };

    // Vertical split of main: log pane | 1-char gap | info bar (2 rows).
    let info_height: u16 = 2;
    let vgap: u16 = 1;

    let log_area = if main_area.height > info_height + vgap {
        Rect {
            x: main_area.x,
            y: main_area.y,
            width: main_area.width,
            height: main_area.height - info_height - vgap,
        }
    } else {
        Rect::default()
    };

    let info_area = Rect {
        x: main_area.x,
        y: main_area.y + main_area.height - info_height,
        width: main_area.width,
        height: info_height.min(main_area.height),
    };

    // Store layout for mouse hit-testing.
    app.layout.sidebar = sidebar_area;
    app.layout.logs = log_area;
    app.layout.info_bar = info_area;

    sidebar::draw(f, app, sidebar_area);
    if log_area.height > 0 {
        log_view::draw_logs(f, app, log_area);
    }
    log_view::draw_header(f, app, info_area);
    command_menu::draw(f, app);
    draw_toast(f, app);
}

/// Draw a toast notification in the bottom-right corner if one is active.
fn draw_toast(f: &mut Frame, app: &App) {
    let toast = match &app.toast {
        Some(t) => t,
        None => return,
    };

    let area = f.area();
    let msg = &toast.message;
    // Pad with spaces on each side.
    let width = (msg.len() as u16) + 4;
    let height = 1u16;

    if area.width < width + 2 || area.height < height + 2 {
        return;
    }

    let toast_area = Rect {
        x: area.x + area.width - width - 1,
        y: area.y + area.height - height - 2,
        width,
        height,
    };

    f.render_widget(Clear, toast_area);

    let bg = Style::default()
        .bg(Color::Rgb(50, 160, 80))
        .fg(Color::White);
    let line = Line::from(vec![Span::styled(format!("  {}  ", msg), bg)]);
    let paragraph = Paragraph::new(line).block(Block::default().style(bg));
    f.render_widget(paragraph, toast_area);
}
