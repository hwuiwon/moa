//! Restate virtual objects hosted by the orchestrator binary.

pub mod cron_job;
pub mod ingestion;
pub mod session;
pub mod tenant;
pub mod worker;

use chrono::{DateTime, Utc};
use restate_sdk::prelude::*;

/// Durably samples the current UTC time inside a virtual object as a replayable step.
///
/// Wrapping the sample in `ctx.run` journals the value so it stays stable across replays.
pub(crate) async fn durable_utc_now(
    ctx: &ObjectContext<'_>,
) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .await?
        .into_inner())
}
