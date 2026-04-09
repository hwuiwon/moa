//! Standalone TUI binary entry point.

use moa_core::MoaConfig;

/// Runs the standalone `moa-tui` binary.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let config = MoaConfig::load()?;
    moa_tui::run_tui(config).await?;
    Ok(())
}
