//! Tenant budget enforcement helpers for streamed brain turns.

use std::sync::Arc;

use chrono::{Duration, Utc};
use moa_core::{
    error::MoaError, error::Result, events::Event, traits::SessionStore,
    types::events_stream::EventRecord, types::identifiers::SessionId, types::identifiers::TenantId,
};
use tokio::sync::broadcast;

use super::context_build::append_event;
use crate::runtime_events::RuntimeEvent;

/// Fails the turn when the tenant has already spent its daily budget.
///
/// `budget_cents` is always enforced: there is no value that disables the
/// check. A zero budget means zero allowed spend, not "unlimited".
pub(super) async fn enforce_tenant_budget(
    session_store: &Arc<dyn SessionStore>,
    session_id: &SessionId,
    tenant_id: &TenantId,
    budget_cents: u32,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    event_tx: Option<&broadcast::Sender<EventRecord>>,
) -> Result<()> {
    let now = Utc::now();
    let day_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|value| value.and_utc());
    let Some(day_start) = day_start else {
        return Ok(());
    };

    let spent = session_store
        .tenant_cost_since(tenant_id, day_start)
        .await?;
    if spent < budget_cents {
        return Ok(());
    }

    let message = format_budget_exhausted_message(budget_cents, now, day_start);
    append_event(
        session_store,
        event_tx,
        *session_id,
        Event::Error {
            message: message.clone(),
            recoverable: false,
        },
    )
    .await?;
    if let Err(err) = runtime_tx.send(RuntimeEvent::Error(message.clone())) {
        tracing::warn!(?err, "runtime receiver dropped while sending Error");
    }
    Err(MoaError::BudgetExhausted(message))
}

fn format_budget_exhausted_message(
    budget_cents: u32,
    now: chrono::DateTime<Utc>,
    day_start: chrono::DateTime<Utc>,
) -> String {
    let budget_dollars = f64::from(budget_cents) / 100.0;
    let remaining = (day_start + Duration::days(1)) - now;
    let remaining_minutes = remaining.num_minutes().max(0);
    let hours = remaining_minutes / 60;
    let minutes = remaining_minutes % 60;

    format!(
        "Daily tenant budget exhausted (${budget_dollars:.2}/day). {hours} hours {minutes} minutes until reset."
    )
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::format_budget_exhausted_message;

    #[test]
    fn budget_message_includes_limit_and_reset_window() {
        let now = Utc.with_ymd_and_hms(2026, 4, 13, 19, 37, 0).unwrap();
        let day_start = Utc.with_ymd_and_hms(2026, 4, 13, 0, 0, 0).unwrap();

        assert_eq!(
            format_budget_exhausted_message(2_000, now, day_start),
            "Daily tenant budget exhausted ($20.00/day). 4 hours 23 minutes until reset."
        );
    }
}
