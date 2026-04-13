//! Dedicated tokio runtime managed for the desktop app's lifetime.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::runtime::{Builder, Runtime};

/// Builds the multi-threaded tokio runtime backing all MOA backend calls.
///
/// The runtime lives for the duration of the process — dropping the returned
/// [`Arc`] would shut it down and cancel outstanding backend tasks.
pub fn build_tokio_runtime() -> Result<Arc<Runtime>> {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("moa-backend")
        .build()
        .context("failed to build tokio runtime for moa backend")?;
    Ok(Arc::new(runtime))
}
