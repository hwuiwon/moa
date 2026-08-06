//! PostgreSQL-backed `SessionStore` implementation.
use std::time::Duration;
use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use chrono::{DateTime, Utc};
use moa_config::MoaConfig;
use moa_config::SessionAttachmentStorageConfig;
use moa_core::traits::{
    SessionAnalyticsStore, SessionChannelBindingUpdate, SessionChannelStore,
    SessionEventLookupStore, SessionLearningLogStore,
};
use moa_core::types::experience::LearningCandidateSummary;
use moa_core::{
    analytics::CacheDailyMetric, analytics::SessionAnalyticsSummary, analytics::SessionTurnMetric,
    analytics::TenantAnalyticsSummary, analytics::ToolCallSummary, error::MoaError, error::Result,
    events::Event, events::EventType, session_replay::record_session_event_replay,
    traits::BlobStore, traits::ExperienceStore, traits::LearningCandidateStore,
    traits::SegmentStore, traits::SessionStore, types::action_policy::ActionPolicyRule,
    types::channel::ChannelAccountId, types::channel::ChannelRef,
    types::channel::SessionChannelBinding, types::channel::SessionChannelBindingId,
    types::contact::ContactId, types::contact::ContactPointId, types::events_stream::ClaimCheck,
    types::events_stream::EventFilter, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::experience::ExperienceAttribution,
    types::experience::ExperienceRecord, types::experience::LearningCandidate,
    types::experience::LearningCandidateStatus, types::experience::LearningCandidateStatusUpdate,
    types::experience::TaskStrategySuccessRate, types::identifiers::SegmentId,
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::ToolCallId, types::learning::LearningEntry,
    types::segment_assessment::SegmentAssessment, types::segment_assessment::SegmentBaseline,
    types::segment_assessment::SkillResolutionRate, types::segments::SegmentCompletion,
    types::segments::TaskSegment, types::session::SessionFilter, types::session::SessionMeta,
    types::session::SessionStatus, types::session::SessionSummary,
    types::snapshot::ContextSnapshot,
};
use moa_observability::{
    SessionEventAppendPhase, record_session_event_append,
    record_session_event_append_phase_duration, record_sessions_active, record_turn_completed,
};
use moa_security::ActionPolicyRuleStore;
use sqlx::{Acquire, PgPool, Postgres, QueryBuilder, Row, postgres::PgPoolOptions, types::Json};
use tracing::warn;

/// Deployment-global advisory-lock key that single-flights the analytics
/// materialized-view refresh across overlapping cron runs and edge replicas.
const ANALYTICS_MV_REFRESH_LOCK_KEY: i64 = 4_924_002_001;
use uuid::Uuid;

use crate::attachment_storage::AttachmentObjectStore;
use crate::blob::{
    FileBlobStore, blob_store_from_config, decode_event_from_storage, encode_event_for_storage,
    preview_text,
};
use crate::queries::{
    EVENT_COLUMNS, EXPERIENCE_ATTRIBUTION_COLUMNS, EXPERIENCE_RECORD_COLUMNS,
    LEARNING_CANDIDATE_COLUMNS, LEARNING_ENTRY_COLUMNS, RowExt, SESSION_INSERT_COLUMNS,
    SESSION_SELECT_COLUMNS, SESSION_SUMMARY_COLUMNS, TASK_SEGMENT_COLUMNS,
    TASK_STRATEGY_SUCCESS_RATE_COLUMNS, action_policy_rule_from_row, call_origin_json,
    experience_attribution_from_row, experience_record_from_row, from_db,
    learning_candidate_from_row, learning_entry_from_row, map_sqlx_error, session_meta_from_row,
    session_summary_from_row, task_segment_from_row, task_strategy_success_rate_from_row,
};
mod action_policy;
mod dashboard;
mod embeddings;
mod experience;
mod helpers;
mod learning;
mod provenance;
mod recurrence;
mod regression;
mod segments;
mod session_archive;
mod session_attachments;
mod session_channels;
mod session_events;
mod session_records;
mod session_store;

use helpers::*;

pub use dashboard::{
    DashboardEventCursor, DashboardEventPage, DashboardEventPageRequest,
    DashboardEventTimelineItem, DashboardSessionDetail, DashboardSessionListCursor,
    DashboardSessionListPage, DashboardSessionListRequest,
};
pub use embeddings::{ExperienceEmbeddingNeighbor, MissingTaskEmbedding, OpenProposalSource};
pub use recurrence::{
    RecurrenceExperienceMember, RecurringExperienceCluster, SkillCandidateDecision,
};
pub use regression::{RecentSkillPromotion, SkillResolutionSample};
pub use session_archive::SessionArchiveStoreError;
pub use session_records::SessionCreateOutcome;

fn local_rustfs_config() -> MoaConfig {
    let mut config = MoaConfig::default();
    config.session.attachments = SessionAttachmentStorageConfig::local_rustfs();
    config
}

struct SessionStorageBackends {
    blob_store: Arc<dyn BlobStore>,
    blob_threshold_bytes: usize,
    attachment_store: AttachmentObjectStore,
}

/// PostgreSQL-backed implementation of `SessionStore`.
#[derive(Clone)]
pub struct PostgresSessionStore {
    url: String,
    pool: PgPool,
    schema_name: Option<String>,
    blob_store: Arc<dyn BlobStore>,
    blob_threshold_bytes: usize,
    attachment_store: AttachmentObjectStore,
}

/// One event to append via [`PostgresSessionStore::append_events`].
pub struct EventAppend {
    /// The event to persist.
    pub event: Event,
    /// Optional idempotency key. A retried append with the same
    /// `(session_id, dedupe_key)` returns the first persisted record instead of
    /// inserting a second event; `None` always appends.
    pub dedupe_key: Option<String>,
}

/// Request to replace a session's active channel route binding.
pub struct SessionChannelBindingReplacement<'a> {
    /// Tenant that owns the contact and session.
    pub tenant_id: moa_core::types::identifiers::TenantId,
    /// Storage partition that owns the session.
    pub storage_partition_id: &'a StoragePartitionId,
    /// Session whose active channel is changing.
    pub session_id: SessionId,
    /// Contact associated with the route.
    pub contact_id: ContactId,
    /// Channel account used by the route, when applicable.
    pub channel_account_id: Option<ChannelAccountId>,
    /// Contact point backing email or SMS routes, when applicable.
    pub contact_point_id: Option<ContactPointId>,
    /// Concrete channel route.
    pub channel_ref: &'a ChannelRef,
    /// Optional caller-supplied reason.
    pub reason: Option<&'a str>,
}

impl PostgresSessionStore {
    /// Creates a session store from config using the configured `PostgreSQL` pool settings.
    ///
    /// The database must already have the complete central migration history.
    pub async fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::new_with_options_and_config(
            config.database.runtime_url(),
            1,
            config.database.max_connections,
            config.database.connect_timeout_seconds,
            config.database.schema.as_deref(),
            config,
        )
        .await
    }

    /// Creates a session store bound to a schema that is already migrated.
    ///
    /// This connects, validates the central migration history, and configures
    /// the search path. It is only safe when the target schema already contains
    /// the session tables — for example a per-test database cloned from a
    /// pre-migrated template by the test harness.
    pub async fn new_in_existing_schema(database_url: &str, schema_name: &str) -> Result<Self> {
        // Pool is capped well below the compose Postgres server limit
        // (max_connections = 100): several isolated test stores run
        // concurrently in the db lanes, and a 100-connection pool lets one
        // high-concurrency test starve the server's connection slots, which
        // surfaces as `pool timed out while waiting for an open connection`
        // in unrelated tests. Concurrent queries queue on the pool instead.
        Self::new_with_options_and_schema(
            database_url,
            1,
            10,
            60,
            Some(schema_name),
            isolated_test_backends()?,
        )
        .await
    }

    /// Creates a session store from an existing Postgres pool using configured blob storage.
    pub async fn from_existing_pool_with_config(config: &MoaConfig, pool: PgPool) -> Result<Self> {
        validate_migration_history(&pool).await?;
        let blob_store = blob_store_from_config(config, pool.clone()).await?;
        let attachment_store = AttachmentObjectStore::from_config(config)?;
        let store = Self {
            url: config.database.url.clone(),
            pool,
            schema_name: config.database.schema.clone(),
            blob_store,
            blob_threshold_bytes: config.session.blob_threshold_bytes,
            attachment_store,
        };
        store.refresh_active_session_metric().await?;
        store.spawn_active_session_gauge_refresher();
        Ok(store)
    }

    /// Verifies the configured Postgres instance is reachable.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| {
                MoaError::ConfigError(format!(
                    "cannot reach Postgres at {}: {error}. Run `docker-compose up -d` from the repo root, or set database.url to a reachable Postgres instance.",
                    redact_password(&self.url)
                ))
            })
    }

    /// Returns the pooled Postgres connection handle used by the session store.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the optional schema name used for this store.
    pub fn schema_name(&self) -> Option<&str> {
        self.schema_name.as_deref()
    }

    /// Loads one session analytics summary row.
    pub async fn get_session_summary(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<SessionAnalyticsSummary> {
        crate::analytics::get_session_summary(&self.pool, self.schema_name(), session_id).await
    }

    /// Lists per-tool analytics rows, optionally scoped to one tenant.
    pub async fn list_tool_call_summaries(
        &self,
        tenant_id: Option<&moa_core::types::identifiers::TenantId>,
    ) -> Result<Vec<ToolCallSummary>> {
        crate::analytics::list_tool_call_summaries(&self.pool, self.schema_name(), tenant_id).await
    }

    /// Lists per-turn analytics rows for one session.
    pub async fn list_session_turn_metrics(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Vec<SessionTurnMetric>> {
        crate::analytics::list_session_turn_metrics(&self.pool, self.schema_name(), session_id)
            .await
    }

    /// Loads aggregated tenant analytics over a recent day window.
    pub async fn get_tenant_stats(
        &self,
        tenant_id: &TenantId,
        days: u32,
    ) -> Result<TenantAnalyticsSummary> {
        crate::analytics::get_tenant_stats(&self.pool, self.schema_name(), tenant_id, days).await
    }

    /// Lists daily cache trend rows for one tenant.
    pub async fn list_cache_daily_metrics(
        &self,
        tenant_id: &TenantId,
        days: u32,
    ) -> Result<Vec<CacheDailyMetric>> {
        crate::analytics::list_cache_daily_metrics(&self.pool, self.schema_name(), tenant_id, days)
            .await
    }

    /// Lists redacted learning-candidate summaries for one tenant.
    pub async fn list_learning_candidate_summaries(
        &self,
        tenant_id: TenantId,
        status: Option<LearningCandidateStatus>,
        limit: u32,
    ) -> Result<Vec<LearningCandidateSummary>> {
        crate::analytics::list_learning_candidate_summaries(
            &self.pool,
            self.schema_name(),
            tenant_id,
            status,
            limit,
        )
        .await
    }

    /// Refreshes the analytics materialized views under a single-flight lease.
    ///
    /// Refresh ownership belongs to the durable maintenance cron. A Postgres
    /// session-level advisory lock single-flights the work so overlapping cron
    /// runs and edge replicas never rebuild the same views at once: a caller that
    /// cannot take the lease returns without doing work. Each attempt records its
    /// outcome (success, failure, duration) so the edge can report read-model
    /// freshness. `CONCURRENTLY` keeps each view readable during its own rebuild.
    pub async fn refresh_analytics_materialized_views(&self) -> Result<()> {
        let lock_key = self.analytics_mv_refresh_lock_key();
        let mut conn = self.pool.acquire().await.map_err(map_sqlx_error)?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(lock_key)
            .fetch_one(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        if !acquired {
            tracing::debug!(
                "analytics materialized-view refresh skipped; another owner holds the lease"
            );
            return Ok(());
        }

        let started = std::time::Instant::now();
        let result = self.run_analytics_mv_refreshes(conn.as_mut()).await;
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

        // Always release the session-level lease, whatever the refresh outcome.
        if let Err(error) = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(lock_key)
            .execute(conn.as_mut())
            .await
        {
            tracing::warn!(error = %error, "failed to release analytics MV refresh advisory lock");
        }

        // Persist freshness best-effort: a state-write failure must not mask the
        // refresh result the caller (and retry policy) depends on.
        match &result {
            Ok(()) => self.record_analytics_refresh_success(duration_ms).await,
            Err(error) => {
                self.record_analytics_refresh_failure(duration_ms, &error.to_string())
                    .await
            }
        }
        result
    }

    /// Advisory-lock key that single-flights the analytics refresh.
    ///
    /// Production stores share the deployment-global key so every replica
    /// contends for one lease. Schema-isolated test stores derive a per-schema
    /// key so parallel tests do not skip each other's refresh.
    pub fn analytics_mv_refresh_lock_key(&self) -> i64 {
        match &self.schema_name {
            None => ANALYTICS_MV_REFRESH_LOCK_KEY,
            Some(schema) => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                ANALYTICS_MV_REFRESH_LOCK_KEY.hash(&mut hasher);
                schema.hash(&mut hasher);
                hasher.finish() as i64
            }
        }
    }

    async fn run_analytics_mv_refreshes(&self, conn: &mut sqlx::PgConnection) -> Result<()> {
        for view_name in ["session_turn_metrics", "daily_storage_partition_metrics"] {
            let qualified = self.table_name(view_name);
            sqlx::query(&format!(
                "REFRESH MATERIALIZED VIEW CONCURRENTLY {qualified}"
            ))
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        }

        // `analytics.event_fact` is intentionally absent: it is a plain VIEW over
        // the `events` table, so it reflects live data and must not be refreshed.
        for qualified in [
            "analytics.session_fact",
            "analytics.turn_fact",
            "analytics.tool_call_fact",
            "analytics.task_segment_fact",
            "analytics.execution_run_fact",
            "analytics.execution_task_fact",
            "analytics.learning_candidate_fact",
            "analytics.experiment_run_fact",
        ] {
            sqlx::query(&format!(
                "REFRESH MATERIALIZED VIEW CONCURRENTLY {qualified}"
            ))
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        }
        Ok(())
    }

    async fn record_analytics_refresh_success(&self, duration_ms: i64) {
        let query = sqlx::query(
            "INSERT INTO analytics.materialized_view_refresh_state
                 (id, last_success_at, last_duration_ms, last_error, updated_at)
             VALUES (TRUE, now(), $1, NULL, now())
             ON CONFLICT (id) DO UPDATE SET
                 last_success_at = now(),
                 last_duration_ms = $1,
                 last_error = NULL,
                 updated_at = now()",
        )
        .bind(duration_ms)
        .execute(&self.pool)
        .await;
        if let Err(error) = query {
            tracing::warn!(error = %error, "failed to record analytics MV refresh success");
        }
    }

    async fn record_analytics_refresh_failure(&self, duration_ms: i64, error_text: &str) {
        let query = sqlx::query(
            "INSERT INTO analytics.materialized_view_refresh_state
                 (id, last_failure_at, last_duration_ms, last_error, updated_at)
             VALUES (TRUE, now(), $1, $2, now())
             ON CONFLICT (id) DO UPDATE SET
                 last_failure_at = now(),
                 last_duration_ms = $1,
                 last_error = $2,
                 updated_at = now()",
        )
        .bind(duration_ms)
        .bind(error_text)
        .execute(&self.pool)
        .await;
        if let Err(error) = query {
            tracing::warn!(error = %error, "failed to record analytics MV refresh failure");
        }
    }

    async fn new_with_options_and_schema(
        database_url: &str,
        pool_min: u32,
        pool_max: u32,
        connect_timeout_secs: u64,
        schema_name: Option<&str>,
        backends: SessionStorageBackends,
    ) -> Result<Self> {
        let pool = Self::connect_with_retry(
            database_url,
            pool_min,
            pool_max,
            connect_timeout_secs,
            3,
            schema_name,
        )
        .await?;
        validate_migration_history(&pool).await?;
        let store = Self {
            url: database_url.to_string(),
            pool,
            schema_name: schema_name.map(ToOwned::to_owned),
            blob_store: backends.blob_store,
            blob_threshold_bytes: backends.blob_threshold_bytes,
            attachment_store: backends.attachment_store,
        };
        store.refresh_active_session_metric().await?;
        Ok(store)
    }

    async fn new_with_options_and_config(
        database_url: &str,
        pool_min: u32,
        pool_max: u32,
        connect_timeout_secs: u64,
        schema_name: Option<&str>,
        config: &MoaConfig,
    ) -> Result<Self> {
        let pool = Self::connect_with_retry(
            database_url,
            pool_min,
            pool_max,
            connect_timeout_secs,
            3,
            schema_name,
        )
        .await?;
        validate_migration_history(&pool).await?;
        let blob_store = blob_store_from_config(config, pool.clone()).await?;
        let attachment_store = AttachmentObjectStore::from_config(config)?;
        let backends = SessionStorageBackends {
            blob_store,
            blob_threshold_bytes: config.session.blob_threshold_bytes,
            attachment_store,
        };
        let store = Self {
            url: database_url.to_string(),
            pool,
            schema_name: schema_name.map(ToOwned::to_owned),
            blob_store: backends.blob_store,
            blob_threshold_bytes: backends.blob_threshold_bytes,
            attachment_store: backends.attachment_store,
        };
        store.refresh_active_session_metric().await?;
        store.spawn_active_session_gauge_refresher();
        Ok(store)
    }

    async fn connect_with_retry(
        database_url: &str,
        pool_min: u32,
        pool_max: u32,
        connect_timeout_secs: u64,
        max_retries: u32,
        schema_name: Option<&str>,
    ) -> Result<PgPool> {
        let backoff = ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(500))
            .with_max_times(max_retries.saturating_sub(1) as usize);
        let search_path = schema_name
            .map(|schema_name| format!("{}, public", quote_identifier(schema_name)))
            .unwrap_or_else(|| "public".to_string());

        (|| async {
            let search_path = search_path.clone();
            PgPoolOptions::new()
                .min_connections(pool_min)
                .max_connections(pool_max)
                .acquire_timeout(Duration::from_secs(connect_timeout_secs))
                .after_connect(move |conn, _meta| {
                    let search_path = search_path.clone();
                    Box::pin(async move {
                        sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                            .bind(search_path)
                            .execute(conn)
                            .await?;
                        Ok(())
                    })
                })
                .connect(database_url)
                .await
        })
        .retry(backoff)
        .notify(|error, delay| {
            warn!(
                max_retries,
                delay_ms = delay.as_millis(),
                error = %error,
                "postgres connection failed, retrying"
            );
        })
        .await
        .map_err(|error| {
            MoaError::StorageError(format!(
                "postgres connection failed after {max_retries} attempts: {error}"
            ))
        })
    }

    fn table_name(&self, table_name: &str) -> String {
        match &self.schema_name {
            Some(schema_name) => format!(
                "{}.{}",
                quote_identifier(schema_name),
                quote_identifier(table_name)
            ),
            None => table_name.to_string(),
        }
    }

    /// Starts a background task that periodically refreshes the active-session
    /// gauge, so session create/status/delete never run a `COUNT(*)` inline.
    ///
    /// Called only from the production constructors; the task holds a pool handle
    /// and exits once that pool is closed. Schema-isolated test stores skip it.
    fn spawn_active_session_gauge_refresher(&self) {
        const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
        let pool = self.pool.clone();
        let sessions_table = self.table_name("sessions");
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if pool.is_closed() {
                    break;
                }
                match helpers::count_running_sessions(&pool, &sessions_table).await {
                    Ok(active) => moa_observability::record_sessions_active(active),
                    Err(error) => {
                        warn!(%error, "failed to refresh active session gauge");
                    }
                }
            }
        });
    }
}

#[async_trait]
impl SessionAnalyticsStore for PostgresSessionStore {
    async fn get_session_summary(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<SessionAnalyticsSummary> {
        PostgresSessionStore::get_session_summary(self, session_id).await
    }

    async fn list_tool_call_summaries(
        &self,
        tenant_id: Option<&moa_core::types::identifiers::TenantId>,
    ) -> Result<Vec<ToolCallSummary>> {
        PostgresSessionStore::list_tool_call_summaries(self, tenant_id).await
    }

    async fn list_session_turn_metrics(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Vec<SessionTurnMetric>> {
        PostgresSessionStore::list_session_turn_metrics(self, session_id).await
    }

    async fn get_tenant_stats(
        &self,
        tenant_id: &TenantId,
        days: u32,
    ) -> Result<TenantAnalyticsSummary> {
        PostgresSessionStore::get_tenant_stats(self, tenant_id, days).await
    }

    async fn list_cache_daily_metrics(
        &self,
        tenant_id: &TenantId,
        days: u32,
    ) -> Result<Vec<CacheDailyMetric>> {
        PostgresSessionStore::list_cache_daily_metrics(self, tenant_id, days).await
    }

    async fn list_learning_candidate_summaries(
        &self,
        tenant_id: TenantId,
        status: Option<LearningCandidateStatus>,
        limit: u32,
    ) -> Result<Vec<LearningCandidateSummary>> {
        PostgresSessionStore::list_learning_candidate_summaries(self, tenant_id, status, limit)
            .await
    }

    async fn refresh_analytics_materialized_views(&self) -> Result<()> {
        PostgresSessionStore::refresh_analytics_materialized_views(self).await
    }
}

/// Builds the in-memory blob store plus local attachment store used by
/// schema-bound test session stores.
fn isolated_test_backends() -> Result<SessionStorageBackends> {
    let blob_dir = FileBlobStore::default_dir_for_database_path(Path::new(":memory:"))?;
    let blob_store: Arc<dyn BlobStore> = Arc::new(FileBlobStore::new(blob_dir));
    let attachment_store = AttachmentObjectStore::from_config(&local_rustfs_config())?;
    Ok(SessionStorageBackends {
        blob_store,
        blob_threshold_bytes: 65_536,
        attachment_store,
    })
}

async fn validate_migration_history(pool: &PgPool) -> Result<()> {
    moa_migrations::validate_complete_history(pool)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!(
                "postgres migration history validation failed: {error:#}"
            ))
        })
}

#[cfg(test)]
mod tests {
    //! Constructor-level database bootstrap tests.

    use super::{PostgresSessionStore, local_rustfs_config};
    use crate::testing::{cleanup_test_schema, provision_cloned_database};
    use sqlx::postgres::PgPoolOptions;

    #[tokio::test]
    #[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
    async fn configured_schema_rejects_divergent_migration_history_db() {
        // Pins: every constructor validates the complete embedded history and
        // fails closed when even the terminal migration checksum diverges.
        let (database_url, schema_name) = provision_cloned_database()
            .await
            .expect("provision migrated clone");

        let outcome = async {
            let mutation_pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&database_url)
                .await?;
            let mutation = sqlx::query(
                "UPDATE refinery_schema_history SET checksum = $1
                 WHERE version = (SELECT MAX(version) FROM refinery_schema_history)",
            )
            .bind("0")
            .execute(&mutation_pool)
            .await?;
            assert_eq!(mutation.rows_affected(), 1);
            mutation_pool.close().await;

            let mut config = local_rustfs_config();
            config.database.url = database_url.clone();
            config.database.admin_url = None;
            config.database.schema = Some(schema_name.clone());
            config.database.max_connections = 2;

            let constructor = PostgresSessionStore::from_config(&config).await;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(constructor)
        }
        .await;

        let cleanup = cleanup_test_schema(&database_url, &schema_name).await;
        let error = outcome
            .expect("corrupt terminal migration checksum")
            .err()
            .expect("divergent migration history must fail closed");
        cleanup.expect("cleanup cloned database");
        assert!(
            error
                .to_string()
                .contains("postgres migration history validation failed"),
            "unexpected validation error: {error:#}"
        );
    }
}
