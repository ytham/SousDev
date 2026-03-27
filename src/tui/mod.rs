/// TUI module — provides a terminal user interface for monitoring workflows.
///
/// The TUI is launched when `sousdev` is run with no subcommand.  It starts
/// the cron scheduler in the background and renders a live dashboard showing
/// workflow flowcharts in a sidebar and streaming logs in the main pane.
pub mod app;
pub mod events;
pub mod session;
pub mod ui;
pub mod widgets;

use anyhow::Result;
use crate::types::config::HarnessConfig;

/// Launch the TUI dashboard.
///
/// This starts the cron scheduler, sets up terminal raw mode, and runs the
/// ratatui event loop until the user quits.
pub async fn run(config: HarnessConfig, no_workspace: bool) -> Result<()> {
    app::run_app(config, no_workspace).await
}
