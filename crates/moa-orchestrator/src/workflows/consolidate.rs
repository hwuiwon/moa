//! Restate workflow that runs one tenant-visible memory-consolidation pass.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use moa_core::{
    config::MoaConfig, traits::EmbeddingProvider, types::identifiers::TenantId,
    types::learning::LearningEntry,
};
use moa_memory_lifecycle::{
    BackfillStats, ConsolidationOptions, ConsolidationOutcome, DecayStats, DigestStats,
    ExpiryStats, MergeStats, SweepStats, TenantConsolidationCursor,
};
use restate_sdk::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;

use crate::objects::tenant::TenantObjectClient;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;

/// Returns the durable workflow ID for a tenant/date consolidation pass.
#[must_use]
pub fn consolidate_workflow_id(tenant_id: &TenantId, target_date: NaiveDate) -> String {
    format!("{tenant_id}:{target_date}")
}

/// Workflow input for one tenant/date consolidation run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConsolidateRequest {
    /// Tenant whose graph memory should be consolidated.
    pub tenant_id: TenantId,
    /// Logical UTC date this workflow instance owns.
    pub target_date: NaiveDate,
    /// Changelog version observed before this workflow was dispatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_changelog_version: Option<i64>,
}

/// Serializable outcome for one workflow execution.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConsolidateReport {
    /// Tenant that was consolidated.
    pub tenant_id: TenantId,
    /// UTC date slot this workflow instance owns.
    pub target_date: NaiveDate,
    /// Timestamp at which the workflow executed.
    pub ran_at: DateTime<Utc>,
    /// Number of graph records rewritten in place.
    pub records_updated: u64,
    /// Number of memory records deleted.
    pub records_deleted: u64,
    /// Number of relative dates normalized.
    pub relative_dates_normalized: u64,
    /// Number of contradiction rewrites performed.
    pub contradictions_resolved: u64,
    /// Number of confidence decays performed.
    pub confidence_decayed: u64,
    /// Number of duplicate facts merged into canonicals.
    #[serde(default)]
    pub duplicates_merged: u64,
    /// Exact-duplicate groups still active after consolidation.
    #[serde(default)]
    pub duplicates_remaining: u64,
    /// Active facts currently at the confidence floor.
    #[serde(default)]
    pub confidence_at_floor: u64,
    /// Floor-bound idle facts closed by bitemporal expiry.
    #[serde(default)]
    pub facts_expired: u64,
    /// Entity nodes that received missing embeddings.
    #[serde(default)]
    pub entity_embeddings_backfilled: u64,
    /// Alias mentions promoted onto Entity node properties.
    #[serde(default)]
    pub aliases_promoted: u64,
    /// Digest rows rebuilt or inserted.
    #[serde(default)]
    pub digests_rebuilt: u64,
    /// Digest rows skipped because they were fresher than the rebuild interval.
    #[serde(default)]
    pub digests_skipped_fresh: u64,
    /// Orphaned graph record identifiers detected during the pass.
    pub orphaned_records: Vec<String>,
    /// Summary record count before consolidation.
    pub summary_records_before: u64,
    /// Summary record count after consolidation.
    pub summary_records_after: u64,
    /// End-to-end workflow duration in milliseconds.
    pub duration_ms: u64,
    /// Non-fatal errors encountered while consolidating.
    pub errors: Vec<String>,
}

impl ConsolidateReport {
    /// Builds a successful no-op report for graph memory.
    #[must_use]
    pub fn graph_noop(
        tenant_id: TenantId,
        target_date: NaiveDate,
        ran_at: DateTime<Utc>,
        duration_ms: u64,
    ) -> Self {
        Self {
            tenant_id,
            target_date,
            ran_at,
            records_updated: 0,
            records_deleted: 0,
            relative_dates_normalized: 0,
            contradictions_resolved: 0,
            confidence_decayed: 0,
            duplicates_merged: 0,
            duplicates_remaining: 0,
            confidence_at_floor: 0,
            facts_expired: 0,
            entity_embeddings_backfilled: 0,
            aliases_promoted: 0,
            digests_rebuilt: 0,
            digests_skipped_fresh: 0,
            orphaned_records: Vec::new(),
            summary_records_before: 0,
            summary_records_after: 0,
            duration_ms,
            errors: Vec::new(),
        }
    }

    /// Builds a failure report that still lets the tenant reschedule future runs.
    #[must_use]
    pub fn failed(
        tenant_id: TenantId,
        target_date: NaiveDate,
        ran_at: DateTime<Utc>,
        duration_ms: u64,
        error: impl Into<String>,
    ) -> Self {
        Self {
            tenant_id,
            target_date,
            ran_at,
            records_updated: 0,
            records_deleted: 0,
            relative_dates_normalized: 0,
            contradictions_resolved: 0,
            confidence_decayed: 0,
            duplicates_merged: 0,
            duplicates_remaining: 0,
            confidence_at_floor: 0,
            facts_expired: 0,
            entity_embeddings_backfilled: 0,
            aliases_promoted: 0,
            digests_rebuilt: 0,
            digests_skipped_fresh: 0,
            orphaned_records: Vec::new(),
            summary_records_before: 0,
            summary_records_after: 0,
            duration_ms,
            errors: vec![error.into()],
        }
    }

    /// Builds a report from lifecycle operation outcomes.
    #[must_use]
    pub fn from_outcome(
        tenant_id: TenantId,
        target_date: NaiveDate,
        ran_at: DateTime<Utc>,
        duration_ms: u64,
        outcome: ConsolidationOutcome,
    ) -> Self {
        let records_updated = outcome.merged
            + outcome.decayed
            + outcome.expired_idle
            + outcome.contradiction_supersessions
            + outcome.entity_embeddings_backfilled
            + outcome.aliases_promoted
            + outcome.digests_rebuilt;
        Self {
            tenant_id,
            target_date,
            ran_at,
            records_updated,
            records_deleted: 0,
            relative_dates_normalized: 0,
            contradictions_resolved: outcome.contradiction_supersessions,
            confidence_decayed: outcome.decayed,
            duplicates_merged: outcome.merged,
            duplicates_remaining: outcome.duplicates_remaining,
            confidence_at_floor: outcome.at_floor,
            facts_expired: outcome.expired_idle,
            entity_embeddings_backfilled: outcome.entity_embeddings_backfilled,
            aliases_promoted: outcome.aliases_promoted,
            digests_rebuilt: outcome.digests_rebuilt,
            digests_skipped_fresh: outcome.digests_skipped_fresh,
            orphaned_records: Vec::new(),
            summary_records_before: 0,
            summary_records_after: 0,
            duration_ms,
            errors: Vec::new(),
        }
    }
}

/// Restate workflow surface for one-shot tenant consolidation runs.
#[restate_sdk::workflow]
pub trait Consolidate {
    /// Runs one durable tenant consolidation pass.
    async fn run(
        request: Json<ConsolidateRequest>,
    ) -> Result<Json<ConsolidateReport>, HandlerError>;
}

/// Concrete workflow implementation.
#[derive(Clone)]
pub struct ConsolidateImpl {
    pool: PgPool,
    config: Arc<MoaConfig>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    session_store: Arc<PostgresSessionStore>,
}

impl ConsolidateImpl {
    /// Creates a consolidation workflow with its storage and memory dependencies.
    #[must_use]
    pub fn new(
        pool: PgPool,
        config: Arc<MoaConfig>,
        embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
        session_store: Arc<PostgresSessionStore>,
    ) -> Self {
        Self {
            pool,
            config,
            embedding_provider,
            session_store,
        }
    }
}

impl Consolidate for ConsolidateImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ConsolidateRequest>,
    ) -> Result<Json<ConsolidateReport>, HandlerError> {
        annotate_restate_handler_span("Consolidate", "run");
        let request = request.into_inner();
        let mut steps = RestateConsolidateSteps {
            ctx: &ctx,
            pool: self.pool.clone(),
            config: self.config.clone(),
            embedding_provider: self.embedding_provider.clone(),
            session_store: self.session_store.clone(),
        };
        let report = run_consolidate_workflow(&mut steps, request).await?;

        Ok(Json::from(report))
    }
}

/// Durable operations used by the consolidation workflow body.
#[async_trait]
pub trait ConsolidateDurableSteps {
    /// Records that the owning tenant has started a consolidation run.
    async fn mark_consolidation_started(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<(), HandlerError>;

    /// Captures the pass timestamp behind a journaled durable step.
    async fn capture_now(&mut self) -> Result<DateTime<Utc>, HandlerError>;

    /// Captures the current tenant changelog version behind a journaled durable step.
    async fn capture_current_changelog_version(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<i64, HandlerError>;

    /// Runs the exact-duplicate merge step.
    async fn merge_duplicates(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<MergeStats, HandlerError>;

    /// Runs the anchored confidence decay step.
    async fn decay_confidence(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<DecayStats, HandlerError>;

    /// Runs the deterministic contradiction sweep step.
    async fn sweep_contradictions(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<SweepStats, HandlerError>;

    /// Runs the idle floor-bound fact expiry step.
    async fn expire_idle_facts(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<ExpiryStats, HandlerError>;

    /// Runs the entity embedding and alias backfill step.
    async fn backfill_entities(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<BackfillStats, HandlerError>;

    /// Runs the deterministic standing digest rebuild step.
    async fn rebuild_digests(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<DigestStats, HandlerError>;

    /// Builds the final consolidation report behind a journaled durable step.
    async fn build_consolidate_report(
        &mut self,
        request: &ConsolidateRequest,
        ran_at: DateTime<Utc>,
        outcome: ConsolidationOutcome,
    ) -> Result<ConsolidateReport, HandlerError>;

    /// Persists any memory-learning entry derived from the report.
    async fn record_memory_learning(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError>;

    /// Records that the owning tenant has completed the consolidation run.
    async fn consolidation_completed(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError>;

    /// Advances the tenant consolidation watermark after successful completion.
    async fn advance_consolidation_watermark(
        &mut self,
        request: &ConsolidateRequest,
        changelog_version: i64,
    ) -> Result<(), HandlerError>;
}

/// Runs the consolidation workflow body against a durable-step implementation.
pub async fn run_consolidate_workflow(
    steps: &mut impl ConsolidateDurableSteps,
    request: ConsolidateRequest,
) -> Result<ConsolidateReport, HandlerError> {
    let observed_changelog_version = match request.observed_changelog_version {
        Some(changelog_version) => changelog_version,
        None => steps.capture_current_changelog_version(&request).await?,
    };
    steps.mark_consolidation_started(&request).await?;
    let ran_at = steps.capture_now().await?;
    let merge = steps.merge_duplicates(&request, ran_at).await?;
    let decay = steps.decay_confidence(&request, ran_at).await?;
    let sweep = steps.sweep_contradictions(&request, ran_at).await?;
    // Expiry runs after decay and the sweep so it sees post-decay confidence and
    // does not close a fact another step would have superseded this run.
    let expiry = steps.expire_idle_facts(&request, ran_at).await?;
    let backfill = steps.backfill_entities(&request).await?;
    let digest = steps.rebuild_digests(&request, ran_at).await?;
    let outcome = ConsolidationOutcome {
        merged: merge.merged,
        decayed: decay.decayed,
        at_floor: decay.at_floor,
        expired_idle: expiry.expired_idle,
        contradiction_supersessions: sweep.contradiction_supersessions,
        entity_embeddings_backfilled: backfill.entity_embeddings_backfilled,
        aliases_promoted: backfill.aliases_promoted,
        duplicates_remaining: merge.duplicates_remaining,
        digests_rebuilt: digest.digests_rebuilt,
        digests_skipped_fresh: digest.digests_skipped_fresh,
    };
    let report = steps
        .build_consolidate_report(&request, ran_at, outcome)
        .await?;
    steps.record_memory_learning(&report).await?;
    steps.consolidation_completed(&report).await?;
    steps
        .advance_consolidation_watermark(&request, observed_changelog_version)
        .await?;
    Ok(report)
}

struct RestateConsolidateSteps<'ctx, 'workflow> {
    ctx: &'ctx WorkflowContext<'workflow>,
    pool: PgPool,
    config: Arc<MoaConfig>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    session_store: Arc<PostgresSessionStore>,
}

#[async_trait]
impl ConsolidateDurableSteps for RestateConsolidateSteps<'_, '_> {
    async fn mark_consolidation_started(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<(), HandlerError> {
        self.ctx
            .object_client::<TenantObjectClient>(request.tenant_id.to_string())
            .mark_consolidation_started(Json::from(request.target_date))
            .call()
            .await
            .map_err(HandlerError::from)
    }

    async fn capture_now(&mut self) -> Result<DateTime<Utc>, HandlerError> {
        self.ctx
            .run(|| async move { Ok(Json::from(Utc::now())) })
            .name("now")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn capture_current_changelog_version(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<i64, HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = request.tenant_id;
        self.ctx
            .run(|| async move {
                moa_memory_lifecycle::tenant_changelog_version(&pool, &tenant_id)
                    .await
                    .map(Json::from)
                    .map_err(lifecycle_handler_error)
            })
            .name("capture_changelog_version")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn merge_duplicates(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<MergeStats, HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = request.tenant_id;
        self.ctx
            .run(|| async move {
                moa_memory_lifecycle::merge_duplicates(&pool, &tenant_id, now)
                    .await
                    .map(Json::from)
                    .map_err(lifecycle_handler_error)
            })
            .name("merge")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn decay_confidence(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<DecayStats, HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = request.tenant_id;
        self.ctx
            .run(|| async move {
                moa_memory_lifecycle::decay_confidence(
                    &pool,
                    &tenant_id,
                    now,
                    &ConsolidationOptions::default(),
                )
                .await
                .map(Json::from)
                .map_err(lifecycle_handler_error)
            })
            .name("decay")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn sweep_contradictions(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<SweepStats, HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = request.tenant_id;
        self.ctx
            .run(|| async move {
                moa_memory_lifecycle::sweep_contradictions(&pool, &tenant_id, now)
                    .await
                    .map(Json::from)
                    .map_err(lifecycle_handler_error)
            })
            .name("contradict")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn expire_idle_facts(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<ExpiryStats, HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = request.tenant_id;
        self.ctx
            .run(|| async move {
                moa_memory_lifecycle::expire_idle_facts(
                    &pool,
                    &tenant_id,
                    now,
                    &ConsolidationOptions::default(),
                )
                .await
                .map(Json::from)
                .map_err(lifecycle_handler_error)
            })
            .name("expire")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn backfill_entities(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<BackfillStats, HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = request.tenant_id;
        let embedder = self.embedding_provider.clone();
        self.ctx
            .run(|| async move {
                moa_memory_lifecycle::backfill_entities(&pool, &tenant_id, embedder)
                    .await
                    .map(Json::from)
                    .map_err(lifecycle_handler_error)
            })
            .name("backfill")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn rebuild_digests(
        &mut self,
        request: &ConsolidateRequest,
        now: DateTime<Utc>,
    ) -> Result<DigestStats, HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = request.tenant_id;
        let digest_config = self.config.memory.digest.clone();
        self.ctx
            .run(|| async move {
                moa_memory_lifecycle::rebuild_digests(&pool, &tenant_id, now, &digest_config)
                    .await
                    .map(Json::from)
                    .map_err(lifecycle_handler_error)
            })
            .name("digest")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn build_consolidate_report(
        &mut self,
        request: &ConsolidateRequest,
        ran_at: DateTime<Utc>,
        outcome: ConsolidationOutcome,
    ) -> Result<ConsolidateReport, HandlerError> {
        let request = request.clone();
        self.ctx
            .run(|| async move {
                Ok(Json::from(ConsolidateReport::from_outcome(
                    request.tenant_id,
                    request.target_date,
                    ran_at,
                    0,
                    outcome,
                )))
            })
            .name("report")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn record_memory_learning(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError> {
        record_memory_learning(self.ctx, self.session_store.clone(), report).await
    }

    async fn consolidation_completed(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError> {
        self.ctx
            .object_client::<TenantObjectClient>(report.tenant_id.to_string())
            .consolidation_completed(Json::from(report.clone()))
            .call()
            .await
            .map_err(HandlerError::from)
    }

    async fn advance_consolidation_watermark(
        &mut self,
        request: &ConsolidateRequest,
        changelog_version: i64,
    ) -> Result<(), HandlerError> {
        let pool = self.pool.clone();
        let cursor = TenantConsolidationCursor {
            tenant_id: request.tenant_id,
            changelog_version,
        };
        self.ctx
            .run(|| async move {
                moa_memory_lifecycle::advance_consolidation_watermark(&pool, &[cursor])
                    .await
                    .map(Json::from)
                    .map_err(lifecycle_handler_error)
            })
            .name("advance_consolidation_watermark")
            .await
            .map(|_| ())
            .map_err(HandlerError::from)
    }
}

async fn record_memory_learning(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    report: &ConsolidateReport,
) -> Result<(), HandlerError> {
    if !report.errors.is_empty() {
        return Ok(());
    }
    if report.records_updated == 0
        && report.records_deleted == 0
        && report.relative_dates_normalized == 0
        && report.contradictions_resolved == 0
        && report.confidence_decayed == 0
        && report.duplicates_merged == 0
        && report.entity_embeddings_backfilled == 0
        && report.aliases_promoted == 0
        && report.digests_rebuilt == 0
    {
        return Ok(());
    }
    let report = report.clone();
    ctx.run(|| async move {
        store
            .append_learning(&LearningEntry {
                id: Uuid::now_v7(),
                tenant_id: report.tenant_id,
                learning_type: "memory_updated".to_string(),
                target_id: report.tenant_id.to_string(),
                target_label: Some("tenant_memory".to_string()),
                payload: serde_json::json!({
                    "target_date": report.target_date,
                    "records_updated": report.records_updated,
                    "records_deleted": report.records_deleted,
                    "relative_dates_normalized": report.relative_dates_normalized,
                    "contradictions_resolved": report.contradictions_resolved,
                    "confidence_decayed": report.confidence_decayed,
                    "duplicates_merged": report.duplicates_merged,
                    "duplicates_remaining": report.duplicates_remaining,
                    "confidence_at_floor": report.confidence_at_floor,
                    "facts_expired": report.facts_expired,
                    "entity_embeddings_backfilled": report.entity_embeddings_backfilled,
                    "aliases_promoted": report.aliases_promoted,
                    "digests_rebuilt": report.digests_rebuilt,
                    "digests_skipped_fresh": report.digests_skipped_fresh,
                }),
                confidence: Some(1.0),
                source_refs: Vec::new(),
                actor: "system".to_string(),
                valid_from: Utc::now(),
                valid_to: None,
                batch_id: None,
                version: 1,
            })
            .await
            .map_err(HandlerError::from)
    })
    .name("record_memory_learning")
    .await?;
    Ok(())
}

fn lifecycle_handler_error(error: moa_memory_lifecycle::consolidate::Error) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
