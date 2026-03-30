/// Top-level draw function — splits the terminal into three columns:
/// Sidebar | Glance | Log pane, with floating overlays on top.
///
/// Layout uses subtle background color differences with 1-char gaps
/// between panels.  Panels extend to the terminal edges.
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::widgets::{command_menu, glance, info_panel, log_view, sidebar};

/// Panel background colors — subtle dark shades to differentiate areas.
pub const BG_SIDEBAR: Color = Color::Rgb(24, 24, 32);
pub const BG_HEADER: Color = Color::Rgb(28, 28, 38);
pub const BG_LOGS: Color = Color::Rgb(20, 20, 28);
/// The terminal default background shows through the 1-char gaps.
pub const BG_GAP: Color = Color::Reset;

/// Draw the full TUI layout.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Horizontal split: sidebar | gap | glance | gap | main pane.
    let sidebar_width: u16 = 26;
    let glance_width: u16 = glance::GLANCE_WIDTH;
    let gap: u16 = 1;
    let left_total = sidebar_width + gap + glance_width + gap;

    if area.width < left_total + 20 {
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

    let glance_area = Rect {
        x: area.x + sidebar_width + gap,
        y: area.y,
        width: glance_width,
        height: area.height,
    };

    let main_x = area.x + left_total;
    let main_width = area.width - left_total;

    // Vertical split of main: log pane | 1-char gap | info bar (2 rows).
    let info_height: u16 = 2;
    let vgap: u16 = 1;

    let log_area = if area.height > info_height + vgap {
        Rect {
            x: main_x,
            y: area.y,
            width: main_width,
            height: area.height - info_height - vgap,
        }
    } else {
        Rect::default()
    };

    let info_area = Rect {
        x: main_x,
        y: area.y + area.height - info_height,
        width: main_width,
        height: info_height.min(area.height),
    };

    // Store layout for mouse hit-testing.
    app.layout.sidebar = sidebar_area;
    app.layout.glance = glance_area;
    app.layout.logs = log_area;
    app.layout.info_bar = info_area;

    // Info panel rect — left-side floating with margins.
    if !app.info_panel_open {
        app.layout.info_panel = Rect::default();
    } else {
        let ipw = info_panel::INFO_PANEL_WIDTH;
        let margin_x: u16 = 10;
        let margin_y: u16 = 2;
        let ip_height = area.height.saturating_sub(margin_y * 2);
        if area.width >= ipw + margin_x + 10 && ip_height >= 8 {
            app.layout.info_panel = Rect {
                x: area.x + margin_x,
                y: area.y + margin_y,
                width: ipw,
                height: ip_height,
            };
        } else {
            app.layout.info_panel = Rect::default();
        }
    }

    // Draw panels.
    sidebar::draw(f, app, sidebar_area);
    glance::draw(f, app, glance_area);
    if log_area.height > 0 {
        log_view::draw_logs(f, app, log_area);
    }
    log_view::draw_header(f, app, info_area);

    // Floating overlays (drawn on top).
    info_panel::draw(f, app);
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
