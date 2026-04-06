/// Top-level draw function — splits the terminal into three columns:
/// Sidebar | Glance | Log pane, with floating overlays on top.
///
/// Layout uses subtle background color differences with 1-char gaps
/// between panels.  Panels extend to the terminal edges.
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::widgets::{command_menu, info, info_expanded, log_view, sidebar};

// ── Theme: Backgrounds (dark to light) ────────────────────────────────────
/// Log pane background (darkest).
pub const BG_LOGS: Color = Color::Rgb(16, 16, 22);
/// Info pane (compact) background.
pub const BG_INFO: Color = Color::Rgb(20, 20, 28);
/// Sidebar background.
pub const BG_SIDEBAR: Color = Color::Rgb(24, 24, 32);
/// Info expanded (floating) panel background.
pub const BG_INFO_EXPANDED: Color = Color::Rgb(28, 28, 38);
/// Menu/popup overlay background — slightly lighter than other dark backgrounds.
pub const BG_MENU: Color = Color::Rgb(35, 35, 45);
/// Status bar background.
pub const BG_STATUS_BAR: Color = Color::Rgb(30, 30, 42);

/// The terminal default background shows through the 1-char gaps.
pub const BG_GAP: Color = Color::Reset;

// ── Theme: Highlight backgrounds ──────────────────────────────────────────
/// Row focus highlight (keyboard cursor in info/info-expanded panels).
pub const BG_ROW_FOCUS: Color = Color::Rgb(40, 40, 54);
/// Mouse text selection highlight (log pane drag-to-copy).
pub const BG_TEXT_SELECTION: Color = Color::Rgb(50, 50, 70);

// ── Theme: Accent colors ──────────────────────────────────────────────────
/// Blue border accent — active panel indicator, floating panel borders.
pub const ACCENT_BORDER: Color = Color::Rgb(80, 110, 200);
/// Cyan-blue — thinking block left border.
pub const ACCENT_THOUGHT: Color = Color::Rgb(80, 160, 200);
/// Subtle background for thinking blocks.
pub const BG_THOUGHT: Color = Color::Rgb(24, 26, 34);
/// Purple — `[tool]` tag in pretty log mode.
pub const ACCENT_TOOL: Color = Color::Rgb(140, 120, 200);
/// Muted blue — "info" log level (replaces raw Color::Blue).
pub const ACCENT_INFO_LEVEL: Color = Color::Rgb(70, 110, 190);
/// Lighter green — toast message area.
pub const ACCENT_TOAST: Color = Color::Rgb(40, 130, 70);
/// Darker green — toast full-width background bar.
pub const BG_TOAST: Color = Color::Rgb(20, 70, 40);

/// Draw the full TUI layout.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Horizontal split: sidebar | gap | info | gap | main pane.
    let sidebar_width: u16 = 26;
    let info_width: u16 = info::INFO_WIDTH;
    let gap: u16 = 1;
    let left_total = sidebar_width + gap + info_width + gap;

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

    let info_pane_area = Rect {
        x: area.x + sidebar_width + gap,
        y: area.y,
        width: info_width,
        height: area.height,
    };

    let main_x = area.x + left_total;
    let main_width = area.width - left_total;

    // Vertical split of main: log pane | 1-char gap | status bar (2 rows).
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

    let status_bar_area = Rect {
        x: main_x,
        y: area.y + area.height - info_height,
        width: main_width,
        height: info_height.min(area.height),
    };

    // Store layout for mouse hit-testing.
    app.layout.sidebar = sidebar_area;
    app.layout.info = info_pane_area;
    app.layout.logs = log_area;
    app.layout.status_bar = status_bar_area;

    // Info expanded panel rect — left-side floating with margins.
    if !app.info_expanded_open {
        app.layout.info_expanded = Rect::default();
    } else {
        let ipw = info_expanded::INFO_EXPANDED_WIDTH;
        let margin_x: u16 = 10;
        if area.width >= ipw + margin_x + 10 && area.height >= 8 {
            app.layout.info_expanded = Rect {
                x: area.x + margin_x,
                y: area.y,
                width: ipw,
                height: area.height,
            };
        } else {
            app.layout.info_expanded = Rect::default();
        }
    }

    // Draw panels.
    sidebar::draw(f, app, sidebar_area);
    info::draw(f, app, info_pane_area);
    if log_area.height > 0 {
        log_view::draw_logs(f, app, log_area);
    }
    log_view::draw_status_bar(f, app, status_bar_area);

    // Floating overlays (drawn on top).
    info_expanded::draw(f, app);
    command_menu::draw(f, app);
    draw_toast(f, app);
}

/// Draw a toast notification as a full-width bar on the 3rd line from
/// the bottom.  The bar has a dark green background, and the message
/// is centered in a lighter green area with 2-space padding on each side.
fn draw_toast(f: &mut Frame, app: &App) {
    let toast = match &app.toast {
        Some(t) => t,
        None => return,
    };

    let area = f.area();
    if area.height < 4 {
        return;
    }

    let msg = &toast.message;
    let pad_x: usize = 2;

    // Full-width bar, 1 line tall, 3rd from bottom.
    let bar = Rect {
        x: area.x,
        y: area.y + area.height - 3,
        width: area.width,
        height: 1,
    };

    f.render_widget(Clear, bar);

    // Build a single line: dark-green fill | lighter-green padded message | dark-green fill.
    let msg_area_width = msg.len() + pad_x * 2;
    let total_width = area.width as usize;

    let dark_style = Style::default().bg(BG_TOAST).fg(Color::White);
    let light_style = Style::default().bg(ACCENT_TOAST).fg(Color::White);

    if msg_area_width >= total_width {
        // Message wider than screen — just render it all in the lighter green.
        let truncated =
            crate::utils::truncate::safe_truncate(msg, total_width.saturating_sub(pad_x * 2));
        let padded = format!("{:^width$}", truncated, width = total_width);
        let line = Line::from(Span::styled(padded, light_style));
        let paragraph = Paragraph::new(vec![line]);
        f.render_widget(paragraph, bar);
    } else {
        let left_fill = (total_width - msg_area_width) / 2;
        let right_fill = total_width - msg_area_width - left_fill;

        let line = Line::from(vec![
            Span::styled(" ".repeat(left_fill), dark_style),
            Span::styled(
                format!("{pad}{msg}{pad}", pad = " ".repeat(pad_x), msg = msg),
                light_style,
            ),
            Span::styled(" ".repeat(right_fill), dark_style),
        ]);
        let paragraph = Paragraph::new(vec![line]);
        f.render_widget(paragraph, bar);
    }
}
