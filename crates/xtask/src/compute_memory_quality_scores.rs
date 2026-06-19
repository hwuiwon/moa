//! `xtask compute-memory-quality-scores` command implementation.

use std::env;

use anyhow::{Context, Result, bail};
use moa_core::WorkspaceId;
use moa_memory_lifecycle::compute_quality_scores;
use sqlx::PgPool;

/// Runs the dark memory quality-score computation job.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for memory quality scoring")?;
    let stats = runtime.block_on(async {
        let pool = PgPool::connect(&options.database_url)
            .await
            .context("connect to Postgres for memory quality scoring")?;
        compute_quality_scores(
            &pool,
            &WorkspaceId::new(options.workspace),
            options.lookback_days,
        )
        .await
        .context("compute memory quality scores")
    })?;
    println!(
        "memory quality scores: scored={} skipped_no_outcome_source={}",
        stats.scored, stats.skipped_no_outcome_source
    );
    Ok(())
}

#[derive(Debug)]
struct Options {
    database_url: String,
    workspace: String,
    lookback_days: i64,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut database_url = None;
        let mut workspace = None;
        let mut lookback_days = 90_i64;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--database-url" => {
                    database_url = Some(args.next().context("--database-url requires a value")?);
                }
                "--workspace" => {
                    workspace = Some(args.next().context("--workspace requires a value")?);
                }
                "--lookback-days" => {
                    let value = args.next().context("--lookback-days requires a value")?;
                    lookback_days = value
                        .parse()
                        .with_context(|| format!("invalid --lookback-days value `{value}`"))?;
                }
                "--help" | "-h" => bail!(usage()),
                other => bail!(
                    "unknown compute-memory-quality-scores argument: {other}\n{}",
                    usage()
                ),
            }
        }

        let database_url = database_url
            .or_else(|| env::var("MOA_DATABASE_URL").ok())
            .context("--database-url or MOA_DATABASE_URL is required")?;
        Ok(Self {
            database_url,
            workspace: workspace.context("--workspace <id> is required")?,
            lookback_days,
        })
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask -- compute-memory-quality-scores --workspace <id> [--database-url <url>] [--lookback-days N]"
}
