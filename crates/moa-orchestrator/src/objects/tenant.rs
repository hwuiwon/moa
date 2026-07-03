//! Restate virtual object that owns one durable tenant orchestration key.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::{StoragePartitionId, TenantId};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::objects::durable_utc_now;
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_scalar};
use crate::workflows::consolidate::{
    ConsolidateClient, ConsolidateReport, ConsolidateRequest, consolidate_workflow_id,
};
use moa_observability::restate_observability::annotate_restate_handler_span;

const K_CONFIG: &str = "config";
const K_LAST_CONSOLIDATION: &str = "last_consolidation";
const K_NEXT_CONSOLIDATION: &str = "next_consolidation";
const K_CONSOLIDATION_IN_PROGRESS: &str = "consolidation_in_progress";

/// Input payload used to initialize a tenant object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TenantConfig {
    /// Tenant identifier.
    pub id: TenantId,
    /// Human-readable tenant name.
    pub name: String,
    /// Hour of day in UTC at which the next consolidation should be scheduled.
    pub consolidation_hour_utc: u8,
}

/// Read-only tenant orchestration status projection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TenantStatus {
    /// Timestamp of the most recent finished consolidation.
    pub last_consolidation_at: Option<DateTime<Utc>>,
    /// Timestamp of the next scheduled consolidation.
    pub next_consolidation_at: Option<DateTime<Utc>>,
    /// Whether a consolidation workflow is currently in progress.
    pub consolidation_in_progress: bool,
    /// Number of graph memory records currently present in the tenant storage partition.
    pub pages_count: u64,
}

/// Serializable projection of the Tenant VO's durable keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TenantVoState {
    /// Tenant configuration payload.
    pub config: Option<TenantConfig>,
    /// Most recent completion timestamp.
    pub last_consolidation: Option<DateTime<Utc>>,
    /// Next scheduled consolidation timestamp.
    pub next_consolidation: Option<DateTime<Utc>>,
    /// Whether a workflow is currently running.
    pub consolidation_in_progress: bool,
}

impl TenantVoState {
    /// Ensures the tenant was initialized before mutating scheduling state.
    pub fn ensure_initialized(&self) -> Result<&TenantConfig, HandlerError> {
        self.config.as_ref().ok_or_else(|| {
            TerminalError::new("tenant not initialized; call Tenant/init first").into()
        })
    }

    /// Marks a consolidation workflow as active without requiring scheduler config.
    pub fn mark_consolidation_started(&mut self) {
        self.consolidation_in_progress = true;
    }

    /// Records a completed consolidation and returns whether a daily VO reschedule is configured.
    pub fn record_consolidation_completed(&mut self, ran_at: DateTime<Utc>) -> bool {
        self.last_consolidation = Some(ran_at);
        self.consolidation_in_progress = false;
        self.config.is_some()
    }
}

impl VoState for TenantVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            config: reader.get_json(K_CONFIG).await?,
            last_consolidation: reader.get_json(K_LAST_CONSOLIDATION).await?,
            next_consolidation: reader.get_json(K_NEXT_CONSOLIDATION).await?,
            consolidation_in_progress: reader
                .get_json(K_CONSOLIDATION_IN_PROGRESS)
                .await?
                .unwrap_or_default(),
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_CONFIG, self.config.as_ref());
        set_or_clear_opt(ctx, K_LAST_CONSOLIDATION, self.last_consolidation.as_ref());
        set_or_clear_opt(ctx, K_NEXT_CONSOLIDATION, self.next_consolidation.as_ref());
        set_or_clear_scalar(
            ctx,
            K_CONSOLIDATION_IN_PROGRESS,
            self.consolidation_in_progress,
            false,
        );
    }
}

/// Returns the next scheduled consolidation time for the given UTC hour.
#[must_use]
pub fn compute_next_consolidation_utc(now: DateTime<Utc>, hour: u8) -> DateTime<Utc> {
    let hour = hour.min(23) as u32;
    let Some(today_target) = now.date_naive().and_hms_opt(hour, 0, 0) else {
        return now;
    };
    let today_target = today_target.and_utc();

    if today_target > now {
        today_target
    } else {
        today_target + chrono::Duration::days(1)
    }
}

/// Returns a stable per-tenant schedule jitter in seconds.
#[must_use]
pub fn deterministic_consolidation_jitter_secs(tenant_id: &TenantId) -> u64 {
    let mut hasher = DefaultHasher::new();
    tenant_id.hash(&mut hasher);
    hasher.finish() % 600
}

/// Restate virtual object surface for one tenant orchestration key.
#[restate_sdk::object]
#[name = "Tenant"]
pub trait TenantObject {
    /// Initializes the tenant object with its persisted config and schedules the first run.
    async fn init(config: Json<TenantConfig>) -> Result<(), HandlerError>;

    /// Schedules the next daily consolidation workflow.
    async fn schedule_consolidation() -> Result<(), HandlerError>;

    /// Marks the tenant as actively consolidating.
    async fn mark_consolidation_started(
        target_date: Json<chrono::NaiveDate>,
    ) -> Result<(), HandlerError>;

    /// Records one completed workflow run and schedules the next run.
    async fn consolidation_completed(report: Json<ConsolidateReport>) -> Result<(), HandlerError>;

    /// Returns read-only scheduling status for the tenant.
    #[shared]
    async fn status() -> Result<Json<TenantStatus>, HandlerError>;
}

/// Concrete `Tenant` virtual object implementation.
pub struct TenantImpl;

impl TenantObject for TenantImpl {
    #[tracing::instrument(skip(self, ctx, config))]
    async fn init(
        &self,
        ctx: ObjectContext<'_>,
        config: Json<TenantConfig>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Tenant", "init");
        let config = config.into_inner();
        validate_tenant_key(ctx.key(), config.id)?;
        validate_consolidation_hour(config.consolidation_hour_utc)?;

        let mut state = TenantVoState::load_from(&ctx).await?;
        state.config = Some(config.clone());
        state.persist_into(&ctx);

        schedule_consolidation_inner(&ctx, &mut state).await?;
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn schedule_consolidation(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Tenant", "schedule_consolidation");
        let mut state = TenantVoState::load_from(&ctx).await?;
        schedule_consolidation_inner(&ctx, &mut state).await?;
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, _target_date))]
    async fn mark_consolidation_started(
        &self,
        ctx: ObjectContext<'_>,
        _target_date: Json<chrono::NaiveDate>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Tenant", "mark_consolidation_started");
        let mut state = TenantVoState::load_from(&ctx).await?;
        state.mark_consolidation_started();
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, report))]
    async fn consolidation_completed(
        &self,
        ctx: ObjectContext<'_>,
        report: Json<ConsolidateReport>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Tenant", "consolidation_completed");
        let report = report.into_inner();
        validate_tenant_key(ctx.key(), report.tenant_id)?;

        let mut state = TenantVoState::load_from(&ctx).await?;
        let should_reschedule = state.record_consolidation_completed(report.ran_at);
        tracing::info!(
            tenant_id = %report.tenant_id,
            target_date = %report.target_date,
            records_updated = report.records_updated,
            records_deleted = report.records_deleted,
            duration_ms = report.duration_ms,
            errors = ?report.errors,
            "tenant consolidation completed"
        );
        if should_reschedule {
            schedule_consolidation_inner(&ctx, &mut state).await?;
        }
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<TenantStatus>, HandlerError> {
        annotate_restate_handler_span("Tenant", "status");
        let state = TenantVoState::load_from(&ctx).await?;
        let tenant_id = tenant_id_from_key(ctx.key())?;
        let pages_count = count_graph_nodes(tenant_id).await?;

        Ok(Json::from(TenantStatus {
            last_consolidation_at: state.last_consolidation,
            next_consolidation_at: state.next_consolidation,
            consolidation_in_progress: state.consolidation_in_progress,
            pages_count,
        }))
    }
}

async fn count_graph_nodes(tenant_id: TenantId) -> Result<u64, HandlerError> {
    let ctx = OrchestratorCtx::current();
    let pool = ctx.graph_pool();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND valid_to IS NULL
        "#,
    )
    .bind(storage_partition_id)
    .fetch_one(&pool)
    .await
    .map_err(HandlerError::from)?;
    Ok(count.max(0) as u64)
}

async fn schedule_consolidation_inner(
    ctx: &ObjectContext<'_>,
    state: &mut TenantVoState,
) -> Result<(), HandlerError> {
    let config = state.ensure_initialized()?.clone();
    let now = durable_utc_now(ctx).await?;
    let next = compute_next_consolidation_utc(now, config.consolidation_hour_utc);
    let jitter_secs = deterministic_consolidation_jitter_secs(&config.id);
    let scheduled_at = next + chrono::Duration::seconds(jitter_secs as i64);
    let delay = scheduled_at.signed_duration_since(now);
    let delay = duration_from_chrono(delay);
    let tenant_id = config.id;
    let workflow_id = consolidate_workflow_id(&tenant_id, next.date_naive());

    state.next_consolidation = Some(scheduled_at);
    ctx.workflow_client::<ConsolidateClient>(workflow_id)
        .run(Json(ConsolidateRequest {
            tenant_id,
            target_date: next.date_naive(),
            observed_changelog_version: None,
        }))
        .send_after(delay);
    tracing::info!(
        tenant_id = %tenant_id,
        scheduled_at = %scheduled_at,
        hour_utc = config.consolidation_hour_utc,
        "scheduled next tenant consolidation"
    );
    Ok(())
}

fn duration_from_chrono(duration: chrono::Duration) -> Duration {
    duration
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(24 * 60 * 60))
}

fn validate_tenant_key(key: &str, tenant_id: TenantId) -> Result<(), HandlerError> {
    if key == tenant_id.to_string() {
        return Ok(());
    }

    Err(TerminalError::new(format!(
        "tenant key `{key}` does not match tenant report id `{tenant_id}`"
    ))
    .into())
}

fn tenant_id_from_key(key: &str) -> Result<TenantId, HandlerError> {
    Uuid::parse_str(key).map(TenantId::from).map_err(|error| {
        TerminalError::new_with_code(
            400,
            format!("tenant object id must be a tenant UUID for consolidation: {error}"),
        )
        .into()
    })
}

fn validate_consolidation_hour(hour: u8) -> Result<(), HandlerError> {
    if hour <= 23 {
        return Ok(());
    }

    Err(TerminalError::new(format!(
        "consolidation hour must be within 0..=23, got {hour}"
    ))
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-15T12:00:00Z")
            .expect("fixed timestamp parses")
            .with_timezone(&Utc)
    }

    #[test]
    fn consolidation_started_accepts_uninitialized_tenant() {
        // Pins: graph-memory cron can mark an active tenant even when Tenant/init never ran.
        let mut state = TenantVoState::default();

        state.mark_consolidation_started();

        assert!(state.consolidation_in_progress);
        assert!(state.config.is_none());
    }

    #[test]
    fn consolidation_completed_uninitialized_tenant_skips_vo_reschedule() {
        // Pins: cron-dispatched consolidation completion does not require daily scheduler config.
        let mut state = TenantVoState {
            consolidation_in_progress: true,
            ..TenantVoState::default()
        };
        let ran_at = fixed_time();

        let should_reschedule = state.record_consolidation_completed(ran_at);

        assert!(!should_reschedule);
        assert_eq!(state.last_consolidation, Some(ran_at));
        assert!(!state.consolidation_in_progress);
    }

    #[test]
    fn consolidation_completed_initialized_tenant_keeps_daily_reschedule() {
        // Pins: initialized Tenant VOs still use their daily self-scheduler after completion.
        let mut state = TenantVoState {
            config: Some(TenantConfig {
                id: TenantId::from(Uuid::from_u128(1)),
                name: "tenant".to_string(),
                consolidation_hour_utc: 3,
            }),
            consolidation_in_progress: true,
            ..TenantVoState::default()
        };

        assert!(state.record_consolidation_completed(fixed_time()));
    }

    #[test]
    fn tenant_state_projection_contains_only_consolidation_state() {
        // Pins: tenant virtual-object state must not grow a policy-rule mirror again.
        let state = TenantVoState {
            config: Some(TenantConfig {
                id: TenantId::from(Uuid::from_u128(1)),
                name: "tenant".to_string(),
                consolidation_hour_utc: 3,
            }),
            last_consolidation: Some(fixed_time()),
            next_consolidation: Some(fixed_time() + chrono::Duration::days(1)),
            consolidation_in_progress: true,
        };

        let value = serde_json::to_value(&state).expect("serialize tenant state");
        let object = value
            .as_object()
            .expect("tenant state serializes as object");

        assert_eq!(object.len(), 4);
        assert!(object.contains_key("config"));
        assert!(object.contains_key("last_consolidation"));
        assert!(object.contains_key("next_consolidation"));
        assert!(object.contains_key("consolidation_in_progress"));
        assert!(!object.contains_key("action_policy"));
    }
}
