//! Periodic graph-memory maintenance triggered by the CronJob virtual object.

use chrono::{NaiveDate, Utc};
use moa_authz::{FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_config::MoaConfig;
use moa_core::traits::Identity;
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::{RebuildLifecycle, RebuildOperationId, RlsContext};
use moa_memory_lifecycle::TenantConsolidationCursor;
use moa_memory_vector::rebuild::RebuildRepository;
use moa_memory_vector::{VectorStoreFactory, VectorSyncReport};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::memory::{
    RebuildActionResponse, RebuildOperationRequest, RebuildStartRequest, RebuildStatusResponse,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;

use crate::handlers::authz_shim::{
    require_configured_fga_client, require_identity, translate_authz_error,
};
use crate::workflows::consolidate::{
    ConsolidateClient, ConsolidateRequest, consolidate_workflow_id,
};
use crate::workflows::knowledge_index_rebuild::{
    KnowledgeIndexRebuildClient, KnowledgeIndexRebuildRequest, knowledge_index_rebuild_workflow_id,
};

/// Request payload for graph-memory compaction.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactRequest {
    /// Optional tenant scope for manual maintenance runs.
    #[serde(default)]
    pub tenant_id: Option<TenantId>,
    /// Optional logical UTC date for manual maintenance runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_date: Option<NaiveDate>,
}

/// Report returned by the graph-memory compaction pass.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompactReport {
    /// Human-readable compaction summary.
    pub summary: String,
    /// Logical UTC date used for queued consolidation workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_date: Option<NaiveDate>,
    /// Number of active graph-memory tenants found for this pass.
    #[serde(default)]
    pub tenants_scanned: u64,
    /// Number of tenant consolidation workflows queued.
    #[serde(default)]
    pub workflows_started: u64,
}

/// Request payload for draining committed vector sync outbox rows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VectorSyncDrainRequest {
    /// Maximum number of outbox rows to claim in one maintenance pass.
    #[serde(default = "default_vector_sync_drain_limit")]
    pub limit: i64,
}

impl Default for VectorSyncDrainRequest {
    fn default() -> Self {
        Self {
            limit: default_vector_sync_drain_limit(),
        }
    }
}

/// Request payload for redriving quarantined (dead-lettered) vector sync rows.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct VectorSyncRedriveRequest {
    /// Optional storage-partition scope; `None` redrives every partition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_partition_id: Option<String>,
}

/// Report returned by a vector-sync redrive pass.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct VectorSyncRedriveReport {
    /// Number of quarantined rows returned to the pending state.
    pub redriven: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsolidationDispatch {
    workflow_id: String,
    request: ConsolidateRequest,
}

/// Keyless Restate service for periodic graph-memory maintenance.
#[restate_sdk::service]
pub trait GraphMemoryMaint {
    /// Runs one graph-memory compaction pass.
    async fn compact(req: Json<CompactRequest>) -> Result<Json<CompactReport>, HandlerError>;

    /// Drains committed vector sync rows into configured external vector backends.
    async fn sync_vectors(
        req: Json<VectorSyncDrainRequest>,
    ) -> Result<Json<VectorSyncReport>, HandlerError>;

    /// Redrives quarantined vector sync rows after an operator remediates the fault.
    ///
    /// Operator-invoked, not cron-scheduled: quarantined (dead-lettered) rows only
    /// return to the drainer when an operator calls this after fixing the
    /// underlying permanent fault (embedder mismatch, backend auth/config).
    async fn redrive_dead_lettered_vectors(
        req: Json<VectorSyncRedriveRequest>,
    ) -> Result<Json<VectorSyncRedriveReport>, HandlerError>;

    /// Starts a durable storage-partition index rebuild.
    async fn start_index_rebuild(
        req: Json<RebuildStartRequest>,
    ) -> Result<Json<RebuildStatusResponse>, HandlerError>;

    /// Reports one rebuild operation's progress, cost estimate, and safe errors.
    async fn index_rebuild_status(
        req: Json<RebuildOperationRequest>,
    ) -> Result<Json<RebuildStatusResponse>, HandlerError>;

    /// Asks a running rebuild to stop at its next committed checkpoint.
    async fn cancel_index_rebuild(
        req: Json<RebuildOperationRequest>,
    ) -> Result<Json<RebuildActionResponse>, HandlerError>;

    /// Restores the retained prior generation as the production read generation.
    async fn rollback_index_rebuild(
        req: Json<RebuildOperationRequest>,
    ) -> Result<Json<RebuildActionResponse>, HandlerError>;

    /// Discards retired generation data, ending the rollback window.
    async fn finalize_index_rebuild(
        req: Json<RebuildOperationRequest>,
    ) -> Result<Json<RebuildActionResponse>, HandlerError>;
}

/// Concrete graph-memory maintenance service implementation.
pub struct GraphMemoryMaintImpl {
    pool: PgPool,
    config: Arc<MoaConfig>,
    fga_client: Option<FgaClient>,
}

impl GraphMemoryMaintImpl {
    /// Creates graph-memory maintenance with its persistence, vector, and
    /// authorization dependencies.
    ///
    /// The authorization client is required by the operator rebuild handlers:
    /// starting, cancelling, rolling back, or finalizing a rebuild changes what
    /// an entire tenant retrieves, so those calls are tenant-admin gated rather
    /// than treated as internal maintenance like the cron handlers above.
    #[must_use]
    pub fn new(pool: PgPool, config: Arc<MoaConfig>, fga_client: Option<FgaClient>) -> Self {
        Self {
            pool,
            config,
            fga_client,
        }
    }
}

impl GraphMemoryMaint for GraphMemoryMaintImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal CronJob/maintenance handler; queues storage compaction workflows only.
    async fn compact(
        &self,
        ctx: Context<'_>,
        request: Json<CompactRequest>,
    ) -> Result<Json<CompactReport>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("GraphMemoryMaint", "compact");
        let request = request.into_inner();
        let target_date = match request.target_date {
            Some(target_date) => target_date,
            None => ctx
                .run(|| async move { Ok::<_, HandlerError>(Json::from(Utc::now().date_naive())) })
                .name("graph-memory-compact-date")
                .await?
                .into_inner(),
        };
        let pool = self.pool.clone();
        let discovery_request = request.clone();
        let tenant_cursors = ctx
            .run(|| async move {
                discover_tenant_cursors(&pool, &discovery_request)
                    .await
                    .map(Json::from)
            })
            .name("graph-memory-compact")
            .await?
            .into_inner();
        let dispatches = build_dispatch_plan(tenant_cursors, target_date);

        for dispatch in &dispatches {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ConsolidateClient>(dispatch.workflow_id.clone())
                    .run(Json::from(dispatch.request.clone())),
            )
            .send();
        }

        let report = compact_report(target_date, dispatches.len() as u64);
        tracing::info!(
            tenant = ?request.tenant_id,
            target_date = %target_date,
            workflows_started = report.workflows_started,
            "graph-memory maintenance queued tenant consolidation"
        );
        Ok(Json::from(report))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal CronJob/maintenance handler; drains durable vector projection outbox rows only.
    async fn sync_vectors(
        &self,
        ctx: Context<'_>,
        request: Json<VectorSyncDrainRequest>,
    ) -> Result<Json<VectorSyncReport>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("GraphMemoryMaint", "sync_vectors");
        let request = request.into_inner();
        if request.limit <= 0 {
            return Err(TerminalError::new("vector sync drain limit must be positive").into());
        }
        let pool = self.pool.clone();
        let config = self.config.clone();
        Ok(ctx
            .run(|| async move {
                let report = VectorStoreFactory::from_config(config.as_ref())
                    .drain_external_sync(&pool, request.limit)
                    .await
                    .map_err(|error| TerminalError::new(format!("drain vector sync: {error}")))?;
                Ok::<_, HandlerError>(Json::from(report))
            })
            .name("graph-memory-vector-sync")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal operator/maintenance handler; re-queues quarantined vector projection outbox rows only.
    async fn redrive_dead_lettered_vectors(
        &self,
        ctx: Context<'_>,
        request: Json<VectorSyncRedriveRequest>,
    ) -> Result<Json<VectorSyncRedriveReport>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("GraphMemoryMaint", "redrive_dead_lettered_vectors");
        let request = request.into_inner();
        let scope_label = request.storage_partition_id.clone();
        let pool = self.pool.clone();
        let config = self.config.clone();
        let partition = request.storage_partition_id;
        let report = ctx
            .run(|| async move {
                let redriven = VectorStoreFactory::from_config(config.as_ref())
                    .redrive_dead_lettered_external_sync(&pool, partition.as_deref())
                    .await
                    .map_err(|error| TerminalError::new(format!("redrive vector sync: {error}")))?;
                Ok::<_, HandlerError>(Json::from(VectorSyncRedriveReport { redriven }))
            })
            .name("graph-memory-vector-redrive")
            .await?
            .into_inner();
        tracing::info!(
            storage_partition_id = ?scope_label,
            redriven = report.redriven,
            "graph-memory maintenance redrove quarantined vector-sync rows"
        );
        Ok(Json::from(report))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn start_index_rebuild(
        &self,
        ctx: Context<'_>,
        request: Json<RebuildStartRequest>,
    ) -> Result<Json<RebuildStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("GraphMemoryMaint", "start_index_rebuild");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        require_rebuild_authority(self.fga_client.clone(), &identity, request.tenant_id).await?;

        // The operation id is minted durably so a retried call resumes the same
        // rebuild rather than racing a second one against the partition.
        let operation_uid = ctx
            .run(|| async { Ok::<_, HandlerError>(Json::from(RebuildOperationId::new())) })
            .name("index_rebuild_operation_id")
            .await?
            .into_inner();

        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<KnowledgeIndexRebuildClient>(
                knowledge_index_rebuild_workflow_id(operation_uid),
            )
            .run(Json::from(KnowledgeIndexRebuildRequest {
                operation_uid,
                tenant_id: request.tenant_id,
                kind: request.kind,
            })),
        )
        .send();

        tracing::info!(
            tenant_id = %request.tenant_id,
            operation_uid = %operation_uid,
            kind = %request.kind,
            "queued storage-partition index rebuild"
        );
        Ok(Json::from(RebuildStatusResponse {
            operation_uid,
            tenant_id: request.tenant_id,
            storage_partition_id: moa_core::types::identifiers::StoragePartitionId::for_tenant(
                request.tenant_id,
            )
            .to_string(),
            kind: request.kind,
            lifecycle: RebuildLifecycle::Planning,
            candidate_generation_uid: None,
            active_generation_uid: None,
            previous_generation_uid: None,
            vectors_total: 0,
            vectors_rebuilt: 0,
            vectors_failed: 0,
            estimated_input_tokens: 0,
            estimated_cost_micros: 0,
            provider_requests: 0,
            provider_throttles: 0,
            provider_retries: 0,
            validation_overlap: None,
            cancel_requested: false,
            last_error_code: None,
            last_error_message: None,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn index_rebuild_status(
        &self,
        ctx: Context<'_>,
        request: Json<RebuildOperationRequest>,
    ) -> Result<Json<RebuildStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("GraphMemoryMaint", "index_rebuild_status");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        require_rebuild_authority(self.fga_client.clone(), &identity, request.tenant_id).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(move || {
                let pool = pool.clone();
                async move {
                    load_rebuild_status(pool, request.tenant_id, request.operation_uid)
                        .await
                        .map(Json::from)
                }
            })
            .name("index_rebuild_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn cancel_index_rebuild(
        &self,
        ctx: Context<'_>,
        request: Json<RebuildOperationRequest>,
    ) -> Result<Json<RebuildActionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("GraphMemoryMaint", "cancel_index_rebuild");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        require_rebuild_authority(self.fga_client.clone(), &identity, request.tenant_id).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(move || {
                let pool = pool.clone();
                async move {
                    let repository =
                        RebuildRepository::new(pool.clone(), RlsContext::tenant(request.tenant_id));
                    // Cooperative: the running build observes the request at its
                    // next batch boundary and stops on a committed checkpoint.
                    let applied = repository
                        .request_cancel(request.operation_uid)
                        .await
                        .map_err(rebuild_handler_error)?;
                    let status =
                        load_rebuild_status(pool, request.tenant_id, request.operation_uid).await?;
                    Ok::<_, HandlerError>(Json::from(RebuildActionResponse {
                        operation_uid: request.operation_uid,
                        lifecycle: status.lifecycle,
                        applied,
                    }))
                }
            })
            .name("index_rebuild_cancel")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn rollback_index_rebuild(
        &self,
        ctx: Context<'_>,
        request: Json<RebuildOperationRequest>,
    ) -> Result<Json<RebuildActionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("GraphMemoryMaint", "rollback_index_rebuild");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        require_rebuild_authority(self.fga_client.clone(), &identity, request.tenant_id).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(move || {
                let pool = pool.clone();
                async move {
                    let repository =
                        RebuildRepository::new(pool.clone(), RlsContext::tenant(request.tenant_id));
                    let operation = repository
                        .load_operation(request.operation_uid)
                        .await
                        .map_err(rebuild_handler_error)?
                        .ok_or_else(|| {
                            TerminalError::new_with_code(404, "rebuild operation not found")
                        })?;
                    if operation.lifecycle != RebuildLifecycle::Activated {
                        return Err(TerminalError::new_with_code(
                            409,
                            "only an activated rebuild can be rolled back",
                        )
                        .into());
                    }
                    let pointer = repository
                        .load_active_generation()
                        .await
                        .map_err(rebuild_handler_error)?
                        .ok_or_else(|| {
                            TerminalError::new_with_code(409, "no active generation to roll back")
                        })?;
                    repository
                        .rollback_generation(pointer.pointer_version)
                        .await
                        .map_err(rebuild_handler_error)?;
                    let rolled_back = repository
                        .transition(
                            request.operation_uid,
                            operation.owner_token,
                            RebuildLifecycle::Activated,
                            RebuildLifecycle::RolledBack,
                        )
                        .await
                        .map_err(rebuild_handler_error)?;
                    Ok::<_, HandlerError>(Json::from(RebuildActionResponse {
                        operation_uid: request.operation_uid,
                        lifecycle: rolled_back.operation().lifecycle,
                        applied: true,
                    }))
                }
            })
            .name("index_rebuild_rollback")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn finalize_index_rebuild(
        &self,
        ctx: Context<'_>,
        request: Json<RebuildOperationRequest>,
    ) -> Result<Json<RebuildActionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("GraphMemoryMaint", "finalize_index_rebuild");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        require_rebuild_authority(self.fga_client.clone(), &identity, request.tenant_id).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(move || {
                let pool = pool.clone();
                async move {
                    let repository =
                        RebuildRepository::new(pool.clone(), RlsContext::tenant(request.tenant_id));
                    let operation = repository
                        .load_operation(request.operation_uid)
                        .await
                        .map_err(rebuild_handler_error)?
                        .ok_or_else(|| {
                            TerminalError::new_with_code(404, "rebuild operation not found")
                        })?;
                    if operation.lifecycle != RebuildLifecycle::Activated {
                        return Err(TerminalError::new_with_code(
                            409,
                            "only an activated rebuild can be finalized",
                        )
                        .into());
                    }
                    let generation_uid = operation.candidate_generation_uid.ok_or_else(|| {
                        TerminalError::new_with_code(409, "rebuild has no activated generation")
                    })?;
                    // After this the retired vectors are gone: rollback is no
                    // longer possible and no reader can reconstruct the retired
                    // contract.
                    repository
                        .finalize_generation(generation_uid)
                        .await
                        .map_err(rebuild_handler_error)?;
                    let finalized = repository
                        .transition(
                            request.operation_uid,
                            operation.owner_token,
                            RebuildLifecycle::Activated,
                            RebuildLifecycle::Finalized,
                        )
                        .await
                        .map_err(rebuild_handler_error)?;
                    Ok::<_, HandlerError>(Json::from(RebuildActionResponse {
                        operation_uid: request.operation_uid,
                        lifecycle: finalized.operation().lifecycle,
                        applied: true,
                    }))
                }
            })
            .name("index_rebuild_finalize")
            .await?)
    }
}

/// Requires tenant-admin authority over the rebuild's own tenant.
///
/// A rebuild rewrites every vector the tenant retrieves from, and activation
/// changes what every member of that tenant sees. Tenant membership is not
/// enough, and the check runs before the operation row is read so an
/// unauthorized caller cannot learn that a rebuild exists.
async fn require_rebuild_authority(
    fga_client: Option<FgaClient>,
    identity: &Identity,
    tenant_id: TenantId,
) -> Result<(), HandlerError> {
    if identity.tenant_id != tenant_id {
        return Err(TerminalError::new_with_code(
            403,
            "rebuild requests are scoped to the caller's own tenant",
        )
        .into());
    }
    let fga = require_configured_fga_client(fga_client)?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

/// Projects durable rebuild state into the operator-visible status shape.
async fn load_rebuild_status(
    pool: PgPool,
    tenant_id: TenantId,
    operation_uid: RebuildOperationId,
) -> Result<RebuildStatusResponse, HandlerError> {
    let repository = RebuildRepository::new(pool, RlsContext::tenant(tenant_id));
    let operation = repository
        .load_operation(operation_uid)
        .await
        .map_err(rebuild_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "rebuild operation not found"))?;
    let pointer = repository
        .load_active_generation()
        .await
        .map_err(rebuild_handler_error)?;
    let validation_overlap = match operation.candidate_generation_uid {
        Some(generation_uid) => repository
            .load_generation(generation_uid)
            .await
            .map_err(rebuild_handler_error)?
            .and_then(|generation| generation.validation_overlap),
        None => None,
    };

    Ok(RebuildStatusResponse {
        operation_uid: operation.operation_uid,
        tenant_id: operation.tenant_id,
        storage_partition_id: operation.storage_partition_id,
        kind: operation.kind,
        lifecycle: operation.lifecycle,
        candidate_generation_uid: operation.candidate_generation_uid,
        active_generation_uid: pointer.as_ref().map(|pointer| pointer.generation_uid),
        previous_generation_uid: pointer
            .as_ref()
            .and_then(|pointer| pointer.previous_generation_uid),
        vectors_total: operation.vectors_total,
        vectors_rebuilt: operation.vectors_rebuilt,
        vectors_failed: operation.vectors_failed,
        estimated_input_tokens: operation.estimated_input_tokens,
        estimated_cost_micros: operation.estimated_cost_micros,
        provider_requests: operation.provider_requests,
        provider_throttles: operation.provider_throttles,
        provider_retries: operation.provider_retries,
        validation_overlap,
        cancel_requested: operation.cancel_requested_at.is_some(),
        last_error_code: operation.last_error_code,
        last_error_message: operation.last_error_message,
    })
}

fn rebuild_handler_error(error: moa_memory_vector::Error) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

fn default_vector_sync_drain_limit() -> i64 {
    512
}

async fn discover_tenant_cursors(
    pool: &PgPool,
    request: &CompactRequest,
) -> Result<Vec<TenantConsolidationCursor>, HandlerError> {
    if let Some(tenant_id) = request.tenant_id {
        let changelog_version = moa_memory_lifecycle::tenant_changelog_version(pool, &tenant_id)
            .await
            .map_err(|error| {
                TerminalError::new(format!("read tenant consolidation cursor: {error}"))
            })?;
        return Ok(vec![TenantConsolidationCursor {
            tenant_id,
            changelog_version,
        }]);
    }

    // Enumerate tenants from the partition registry (`storage_partition_state`),
    // which holds one row per tenant partition, and skip tenants whose graph has
    // not changed since their last consolidation. This replaces a global
    // `SELECT DISTINCT` scan over the whole node index and short-circuits idle
    // tenants entirely.
    moa_memory_lifecycle::tenants_needing_consolidation(pool)
        .await
        .map_err(|error| {
            TerminalError::new(format!("discover consolidation tenants: {error}")).into()
        })
}

fn build_dispatch_plan(
    mut tenant_cursors: Vec<TenantConsolidationCursor>,
    target_date: NaiveDate,
) -> Vec<ConsolidationDispatch> {
    tenant_cursors.sort_by_key(|cursor| cursor.tenant_id.to_string());
    let mut deduped: Vec<TenantConsolidationCursor> = Vec::with_capacity(tenant_cursors.len());
    for cursor in tenant_cursors {
        match deduped.last_mut() {
            Some(previous)
                if previous.tenant_id == cursor.tenant_id
                    && previous.changelog_version < cursor.changelog_version =>
            {
                previous.changelog_version = cursor.changelog_version;
            }
            Some(previous) if previous.tenant_id == cursor.tenant_id => {}
            _ => deduped.push(cursor),
        }
    }

    deduped
        .into_iter()
        .map(|cursor| ConsolidationDispatch {
            workflow_id: consolidate_workflow_id(&cursor.tenant_id, target_date),
            request: ConsolidateRequest {
                tenant_id: cursor.tenant_id,
                target_date,
                observed_changelog_version: Some(cursor.changelog_version),
            },
        })
        .collect()
}

fn compact_report(target_date: NaiveDate, workflows_started: u64) -> CompactReport {
    CompactReport {
        summary: format!(
            "queued {workflows_started} tenant consolidation workflow{}",
            plural_suffix(workflows_started)
        ),
        target_date: Some(target_date),
        tenants_scanned: workflows_started,
        workflows_started,
    }
}

fn plural_suffix(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 12).expect("test date should be valid")
    }

    #[test]
    fn dispatch_plan_sorts_and_deduplicates_tenant_workflows() {
        // Pins: graph maintenance queues exactly one deterministic Consolidate workflow per tenant/date.
        let plan = build_dispatch_plan(
            vec![
                cursor(tenant(2), 12),
                cursor(tenant(1), 7),
                cursor(tenant(1), 9),
            ],
            target_date(),
        );

        let workflow_ids = plan
            .iter()
            .map(|dispatch| dispatch.workflow_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            workflow_ids,
            vec![
                "00000000-0000-0000-0000-000000000001:2026-06-12",
                "00000000-0000-0000-0000-000000000002:2026-06-12"
            ]
        );
        let requests = plan
            .iter()
            .map(|dispatch| {
                (
                    dispatch.request.tenant_id.to_string(),
                    dispatch.request.target_date,
                    dispatch.request.observed_changelog_version,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                (
                    "00000000-0000-0000-0000-000000000001".to_string(),
                    target_date(),
                    Some(9)
                ),
                (
                    "00000000-0000-0000-0000-000000000002".to_string(),
                    target_date(),
                    Some(12)
                )
            ]
        );
    }

    #[test]
    fn compact_report_pins_queued_tenant_counts() {
        // Pins: graph-memory maintenance reports queued tenant workflows without synthetic compatibility counters.
        assert_eq!(
            compact_report(target_date(), 0),
            CompactReport {
                summary: "queued 0 tenant consolidation workflows".to_string(),
                target_date: Some(target_date()),
                tenants_scanned: 0,
                workflows_started: 0,
            }
        );
    }

    #[test]
    fn tenant_filter_is_accepted_for_direct_tenant_dispatch() {
        // Pins: tenant filters are first-class maintenance scope.
        let request = CompactRequest {
            tenant_id: Some(tenant(7)),
            ..CompactRequest::default()
        };

        assert_eq!(request.tenant_id, Some(tenant(7)));
    }

    fn tenant(value: u128) -> TenantId {
        TenantId::from(uuid::Uuid::from_u128(value))
    }

    fn cursor(tenant_id: TenantId, changelog_version: i64) -> TenantConsolidationCursor {
        TenantConsolidationCursor {
            tenant_id,
            changelog_version,
        }
    }
}
