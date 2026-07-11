//! `xtask compute-memory-quality-scores` command implementation.

use std::env;

use anyhow::{Context, Result, bail};
use moa_core::types::identifiers::TenantId;
use moa_memory_lifecycle::compute_quality_scores;
use sqlx::PgPool;
use uuid::Uuid;

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
        let tenant_uuid = Uuid::parse_str(&options.tenant_id)
            .with_context(|| format!("invalid --tenant-id UUID `{}`", options.tenant_id))?;
        let tenant_id = TenantId::from(tenant_uuid);
        compute_quality_scores(&pool, &tenant_id, options.lookback_days)
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
    tenant_id: String,
    lookback_days: i64,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut database_url = None;
        let mut tenant_id = None;
        let mut lookback_days = 90_i64;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--database-url" => {
                    database_url = Some(args.next().context("--database-url requires a value")?);
                }
                "--tenant-id" => {
                    tenant_id = Some(args.next().context("--tenant-id requires a value")?);
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
            tenant_id: tenant_id.context("--tenant-id <id> is required")?,
            lookback_days,
        })
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask --features eval-tools -- compute-memory-quality-scores --tenant-id <id> [--database-url <url>] [--lookback-days N]"
}
