//! Restate workflow that owns one storage-partition index rebuild.
//!
//! The workflow is a sequence of durable steps over the rebuild repository:
//! take a census, create a candidate generation, build it in bounded batches,
//! validate it against the generation it would replace, and activate it.
//!
//! What makes crash and retry safe is not this file — it is the repository
//! contract underneath. Every transition is a compare-and-swap on
//! `(operation, owner, lifecycle)`, candidate rows upsert on their primary key,
//! progress is recounted rather than incremented, and the checkpoint only moves
//! forward. A replayed step therefore lands on an already-applied transition
//! and continues, and a duplicated batch rewrites the rows it already wrote.
//! The workflow can be interrupted anywhere and resumed without duplicating a
//! candidate or letting one generation overwrite another's fence.
//!
//! Cancellation is cooperative. The build checks between batches and stops at a
//! committed checkpoint, because interrupting mid-batch would leave the
//! candidate set in a state whose completeness could not be reasoned about.
//!
//! Failed validation is not an error state to recover from: the old generation
//! was authoritative the entire time and stays that way. Nothing was ever
//! served from the candidate.

use std::sync::Arc;

use moa_core::traits::EmbeddingProvider;
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::{
    EmbeddingGenerationId, RebuildKind, RebuildLifecycle, RebuildOperationId, RlsContext,
};
use moa_memory_vector::rebuild::{
    BatchCommit, BatchCounters, CandidateVector, REBUILD_BATCH_SIZE, REBUILD_OVERLAP_THRESHOLD,
    RebuildFence, RebuildRepository, StartRebuild,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;

/// Deterministic per-million-input-token rate used for cost projection.
///
/// A planning figure, not a bill. The `EmbeddingProvider` trait reports no
/// billed usage, so an operator asking "what will this cost" gets an estimate
/// derived from input size, and every field carrying it says `estimated`.
const ESTIMATE_MICROS_PER_MILLION_TOKENS: i64 = 100_000;

/// Shadow queries issued per validation pass.
const VALIDATION_SAMPLE_LIMIT: i64 = 64;

/// Workflow input for one storage-partition index rebuild.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeIndexRebuildRequest {
    /// Operation this workflow owns. Chosen by the caller so a resubmission
    /// resumes the same operation instead of starting a second one.
    pub operation_uid: RebuildOperationId,
    /// Tenant whose storage partition is rebuilt.
    pub tenant_id: TenantId,
    /// Which rebuild to run.
    pub kind: RebuildKind,
}

/// Report returned when a rebuild workflow finishes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeIndexRebuildReport {
    /// Operation the workflow ran.
    pub operation_uid: RebuildOperationId,
    /// Tenant that owns the partition.
    pub tenant_id: TenantId,
    /// Candidate generation built, when one was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_uid: Option<EmbeddingGenerationId>,
    /// Lifecycle the operation ended in.
    pub lifecycle: RebuildLifecycle,
    /// Partition-wide vector census.
    pub vectors_total: i64,
    /// Candidate vectors written.
    pub vectors_rebuilt: i64,
    /// Source vectors whose input could not be reconstructed.
    pub vectors_failed: i64,
    /// Mean top-K overlap measured by shadow validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_overlap: Option<f64>,
    /// Deterministic cost projection in micros. Not a billed figure.
    pub estimated_cost_micros: i64,
}

/// Restate workflow surface for one storage-partition index rebuild.
#[restate_sdk::workflow]
pub trait KnowledgeIndexRebuild {
    /// Runs one durable rebuild through validation and activation.
    async fn run(
        request: Json<KnowledgeIndexRebuildRequest>,
    ) -> Result<Json<KnowledgeIndexRebuildReport>, HandlerError>;
}

/// Concrete rebuild workflow implementation.
#[derive(Clone)]
pub struct KnowledgeIndexRebuildImpl {
    pool: PgPool,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
}

impl KnowledgeIndexRebuildImpl {
    /// Creates a rebuild workflow with its storage and embedding provider.
    #[must_use]
    pub fn new(pool: PgPool, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self { pool, embedder }
    }
}

/// Returns the deterministic workflow id for one rebuild operation.
///
/// Keyed by the operation rather than the tenant so a resubmission of the same
/// operation attaches to the running workflow instead of starting a rival one.
#[must_use]
pub fn knowledge_index_rebuild_workflow_id(operation_uid: RebuildOperationId) -> String {
    format!("knowledge-index-rebuild:{operation_uid}")
}

impl KnowledgeIndexRebuild for KnowledgeIndexRebuildImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal workflow; the tenant-admin authorization for this rebuild
    // is enforced by the GraphMemoryMaint handler that dispatches it, and the
    // partition is derived from the stored operation row rather than the caller.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<KnowledgeIndexRebuildRequest>,
    ) -> Result<Json<KnowledgeIndexRebuildReport>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("KnowledgeIndexRebuild", "run");
        let request = request.into_inner();
        let embedder = self
            .embedder
            .clone()
            .ok_or_else(|| TerminalError::new("index rebuild requires an embedding provider"))?;

        // One owner token per workflow execution, sampled durably so a replay
        // reuses it. Every compare-and-swap carries it, which is what lets a
        // replayed step be recognized as this execution's own work rather than
        // a rival writer's.
        let owner_token = ctx
            .run(|| async { Ok::<_, HandlerError>(Json::from(Uuid::now_v7())) })
            .name("rebuild_owner_token")
            .await?
            .into_inner();

        let repository =
            RebuildRepository::new(self.pool.clone(), RlsContext::tenant(request.tenant_id));

        let plan = plan_rebuild(&ctx, &repository, &request, owner_token, &embedder).await?;
        let generation_uid = plan.generation_uid;
        let fence = RebuildFence {
            operation_uid: request.operation_uid,
            owner_token,
            generation_uid,
        };

        let mut checkpoint = plan.checkpoint_uid;
        let mut batch_index = plan.checkpoint_batch_index;
        loop {
            let batch = build_batch(
                &ctx,
                &repository,
                fence,
                BuildCursor {
                    checkpoint,
                    batch_index,
                },
                &embedder,
            )
            .await?;
            match batch {
                BatchOutcome::Cancelled => {
                    let cancelled = finish(
                        &ctx,
                        &repository,
                        &request,
                        owner_token,
                        RebuildLifecycle::Building,
                        RebuildLifecycle::Cancelled,
                    )
                    .await?;
                    return Ok(Json::from(
                        report(
                            &ctx,
                            &repository,
                            &request,
                            Some(generation_uid),
                            cancelled,
                            None,
                        )
                        .await?,
                    ));
                }
                BatchOutcome::Exhausted => break,
                BatchOutcome::Advanced {
                    checkpoint_uid,
                    next_batch_index,
                } => {
                    checkpoint = Some(checkpoint_uid);
                    batch_index = next_batch_index;
                }
            }
        }

        let validation = validate(
            &ctx,
            &repository,
            &request,
            owner_token,
            generation_uid,
            plan.vectors_total,
        )
        .await?;

        if !validation.passed {
            // The old generation was authoritative throughout and remains so.
            // Nothing is rolled back because nothing was ever activated.
            let failed = finish(
                &ctx,
                &repository,
                &request,
                owner_token,
                RebuildLifecycle::Validating,
                RebuildLifecycle::Failed,
            )
            .await?;
            return Ok(Json::from(
                report(
                    &ctx,
                    &repository,
                    &request,
                    Some(generation_uid),
                    failed,
                    Some(validation.overlap),
                )
                .await?,
            ));
        }

        let activated = activate(&ctx, &repository, &request, owner_token, generation_uid).await?;

        Ok(Json::from(
            report(
                &ctx,
                &repository,
                &request,
                Some(generation_uid),
                activated,
                Some(validation.overlap),
            )
            .await?,
        ))
    }
}

/// Census and candidate generation produced by the planning step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RebuildPlan {
    generation_uid: EmbeddingGenerationId,
    vectors_total: i64,
    checkpoint_uid: Option<Uuid>,
    checkpoint_batch_index: i64,
}

/// Where the build loop resumes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuildCursor {
    /// Last committed source uid, or `None` before the first batch.
    checkpoint: Option<Uuid>,
    /// Monotonic batch index.
    batch_index: i64,
}

/// Outcome of one bounded build batch.
enum BatchOutcome {
    /// An operator asked the rebuild to stop; progress is committed.
    Cancelled,
    /// No source vectors remain.
    Exhausted,
    /// The batch committed and the checkpoint advanced.
    Advanced {
        checkpoint_uid: Uuid,
        next_batch_index: i64,
    },
}

/// Result of one shadow validation pass.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
struct ValidationOutcome {
    passed: bool,
    overlap: f64,
}

async fn plan_rebuild(
    ctx: &WorkflowContext<'_>,
    repository: &RebuildRepository,
    request: &KnowledgeIndexRebuildRequest,
    owner_token: Uuid,
    embedder: &Arc<dyn EmbeddingProvider>,
) -> Result<RebuildPlan, HandlerError> {
    let repository = repository.clone();
    let embedding_model = embedder.model_id().to_string();
    let embedding_model_version = embedder.model_version();
    let kind = request.kind;
    let operation_uid = request.operation_uid;
    let namespace_prefix = repository.storage_partition_id();

    ctx.run(|| {
        let repository = repository.clone();
        let embedding_model = embedding_model.clone();
        let namespace_prefix = namespace_prefix.clone();
        async move {
            let unsupported = repository
                .unrebuildable_labels()
                .await
                .map_err(rebuild_error)?;
            if !unsupported.is_empty() {
                // Rebuilding a partition while skipping some of its vectors is
                // the mixed-model state this whole path exists to prevent, so a
                // label with no reconstruction rule stops the operation before
                // any candidate is written.
                return Err(TerminalError::new(format!(
                    "storage partition holds vectors this rebuild cannot reconstruct: {}",
                    unsupported.join(", ")
                ))
                .into());
            }
            let vectors_total = repository
                .count_partition_vectors()
                .await
                .map_err(rebuild_error)?;

            repository
                .start_operation(StartRebuild {
                    operation_uid,
                    owner_token,
                    kind,
                    embedding_model: embedding_model.clone(),
                    embedding_model_version,
                    estimate_micros_per_million_tokens: ESTIMATE_MICROS_PER_MILLION_TOKENS,
                })
                .await
                .map_err(rebuild_error)?;
            repository
                .ensure_bootstrap_generation(
                    &embedding_model,
                    embedding_model_version,
                    &namespace_prefix,
                )
                .await
                .map_err(rebuild_error)?;
            repository
                .record_plan(operation_uid, owner_token, vectors_total)
                .await
                .map_err(rebuild_error)?;
            let generation = repository
                .create_candidate_generation(
                    operation_uid,
                    owner_token,
                    EmbeddingGenerationId::new(),
                    &embedding_model,
                    embedding_model_version,
                    &namespace_prefix,
                )
                .await
                .map_err(rebuild_error)?;
            let operation = repository
                .transition(
                    operation_uid,
                    owner_token,
                    RebuildLifecycle::Planning,
                    RebuildLifecycle::Building,
                )
                .await
                .map_err(rebuild_error)?
                .into_operation();

            Ok::<_, HandlerError>(Json::from(RebuildPlan {
                generation_uid: generation.generation_uid,
                vectors_total: operation.vectors_total,
                checkpoint_uid: operation.checkpoint_uid,
                checkpoint_batch_index: operation.checkpoint_batch_index,
            }))
        }
    })
    .name("rebuild_plan")
    .await
    .map(Json::into_inner)
    .map_err(HandlerError::from)
}

async fn build_batch(
    ctx: &WorkflowContext<'_>,
    repository: &RebuildRepository,
    fence: RebuildFence,
    cursor: BuildCursor,
    embedder: &Arc<dyn EmbeddingProvider>,
) -> Result<BatchOutcome, HandlerError> {
    let repository = repository.clone();
    let embedder = embedder.clone();
    let RebuildFence { operation_uid, .. } = fence;
    let BuildCursor {
        checkpoint,
        batch_index,
    } = cursor;

    let committed: Option<Uuid> = ctx
        .run(|| {
            let repository = repository.clone();
            let embedder = embedder.clone();
            async move {
                // Cancellation is observed at a batch boundary, where the
                // committed checkpoint exactly describes what has been built.
                let operation = repository
                    .load_operation(operation_uid)
                    .await
                    .map_err(rebuild_error)?
                    .ok_or_else(|| TerminalError::new("rebuild operation disappeared"))?;
                if operation.cancel_requested_at.is_some() {
                    return Ok::<_, HandlerError>(Json::from(None));
                }

                let inputs = repository
                    .load_authoritative_inputs(checkpoint, REBUILD_BATCH_SIZE)
                    .await
                    .map_err(rebuild_error)?;
                if inputs.is_empty() {
                    return Ok(Json::from(None));
                }

                let texts = inputs
                    .iter()
                    .map(|input| input.text.clone())
                    .collect::<Vec<_>>();
                let vectors = embedder
                    .embed(&texts)
                    .await
                    .map_err(|error| TerminalError::new(format!("rebuild embed: {error}")))?;
                if vectors.len() != texts.len() {
                    return Err(TerminalError::new(format!(
                        "embedding provider returned {} vectors for {} inputs",
                        vectors.len(),
                        texts.len()
                    ))
                    .into());
                }

                let estimated_input_tokens = inputs
                    .iter()
                    .map(|input| i64::from(input.estimated_tokens()))
                    .sum::<i64>();
                let candidates = inputs
                    .iter()
                    .zip(vectors)
                    .map(|(input, vector)| CandidateVector::from_input(input, vector))
                    .collect::<Vec<_>>();
                let last_uid = candidates
                    .last()
                    .map(|candidate| candidate.uid)
                    .ok_or_else(|| TerminalError::new("rebuild batch produced no candidates"))?;

                repository
                    .commit_batch(
                        fence,
                        BatchCommit {
                            candidates: &candidates,
                            checkpoint_uid: last_uid,
                            batch_index,
                            counters: BatchCounters {
                                vectors_failed: 0,
                                estimated_input_tokens,
                                provider_requests: 1,
                                provider_throttles: 0,
                                provider_retries: 0,
                            },
                            estimate_micros_per_million_tokens: ESTIMATE_MICROS_PER_MILLION_TOKENS,
                        },
                    )
                    .await
                    .map_err(rebuild_error)?;
                Ok(Json::from(Some(last_uid)))
            }
        })
        .name(format!("rebuild_batch_{batch_index}"))
        .await?
        .into_inner();

    let Some(checkpoint_uid) = committed else {
        let operation = ctx
            .run(|| {
                let repository = repository.clone();
                async move {
                    repository
                        .load_operation(operation_uid)
                        .await
                        .map_err(rebuild_error)?
                        .map(|operation| operation.cancel_requested_at.is_some())
                        .ok_or_else(|| {
                            HandlerError::from(TerminalError::new("rebuild operation disappeared"))
                        })
                        .map(Json::from)
                }
            })
            .name(format!("rebuild_batch_{batch_index}_cancel_check"))
            .await?
            .into_inner();
        return Ok(if operation {
            BatchOutcome::Cancelled
        } else {
            BatchOutcome::Exhausted
        });
    };

    Ok(BatchOutcome::Advanced {
        checkpoint_uid,
        next_batch_index: batch_index.saturating_add(1),
    })
}

async fn validate(
    ctx: &WorkflowContext<'_>,
    repository: &RebuildRepository,
    request: &KnowledgeIndexRebuildRequest,
    owner_token: Uuid,
    generation_uid: EmbeddingGenerationId,
    vectors_total: i64,
) -> Result<ValidationOutcome, HandlerError> {
    let repository = repository.clone();
    let operation_uid = request.operation_uid;

    ctx.run(|| {
        let repository = repository.clone();
        async move {
            repository
                .mark_generation_complete(generation_uid, vectors_total)
                .await
                .map_err(rebuild_error)?;
            repository
                .transition(
                    operation_uid,
                    owner_token,
                    RebuildLifecycle::Building,
                    RebuildLifecycle::Validating,
                )
                .await
                .map_err(rebuild_error)?;

            let validation = repository
                .validate_candidate_generation(generation_uid, VALIDATION_SAMPLE_LIMIT)
                .await
                .map_err(rebuild_error)?;

            repository
                .record_validation(generation_uid, validation.overlap)
                .await
                .map_err(rebuild_error)?;
            let passed = validation.passes(REBUILD_OVERLAP_THRESHOLD);
            if passed {
                repository
                    .transition(
                        operation_uid,
                        owner_token,
                        RebuildLifecycle::Validating,
                        RebuildLifecycle::AwaitingActivation,
                    )
                    .await
                    .map_err(rebuild_error)?;
            } else {
                repository
                    .record_error(
                        operation_uid,
                        "rebuild_validation_overlap_below_threshold",
                        &format!(
                            "shadow overlap {:.3} is below the {:.3} activation bar",
                            validation.overlap, REBUILD_OVERLAP_THRESHOLD
                        ),
                    )
                    .await
                    .map_err(rebuild_error)?;
            }
            Ok::<_, HandlerError>(Json::from(ValidationOutcome {
                passed,
                overlap: validation.overlap,
            }))
        }
    })
    .name("rebuild_validate")
    .await
    .map(Json::into_inner)
    .map_err(HandlerError::from)
}

async fn activate(
    ctx: &WorkflowContext<'_>,
    repository: &RebuildRepository,
    request: &KnowledgeIndexRebuildRequest,
    owner_token: Uuid,
    generation_uid: EmbeddingGenerationId,
) -> Result<RebuildLifecycle, HandlerError> {
    let repository = repository.clone();
    let operation_uid = request.operation_uid;

    ctx.run(|| {
        let repository = repository.clone();
        async move {
            let pointer = repository
                .load_active_generation()
                .await
                .map_err(rebuild_error)?
                .ok_or_else(|| {
                    TerminalError::new("storage partition has no active generation to replace")
                })?;
            if pointer.generation_uid != generation_uid {
                repository
                    .activate_generation(generation_uid, pointer.pointer_version)
                    .await
                    .map_err(rebuild_error)?;
            }
            let operation = repository
                .transition(
                    operation_uid,
                    owner_token,
                    RebuildLifecycle::AwaitingActivation,
                    RebuildLifecycle::Activated,
                )
                .await
                .map_err(rebuild_error)?
                .into_operation();
            Ok::<_, HandlerError>(Json::from(operation.lifecycle))
        }
    })
    .name("rebuild_activate")
    .await
    .map(Json::into_inner)
    .map_err(HandlerError::from)
}

async fn finish(
    ctx: &WorkflowContext<'_>,
    repository: &RebuildRepository,
    request: &KnowledgeIndexRebuildRequest,
    owner_token: Uuid,
    from: RebuildLifecycle,
    to: RebuildLifecycle,
) -> Result<RebuildLifecycle, HandlerError> {
    let repository = repository.clone();
    let operation_uid = request.operation_uid;
    ctx.run(|| {
        let repository = repository.clone();
        async move {
            let operation = repository
                .transition(operation_uid, owner_token, from, to)
                .await
                .map_err(rebuild_error)?
                .into_operation();
            Ok::<_, HandlerError>(Json::from(operation.lifecycle))
        }
    })
    .name(format!("rebuild_finish_{to}"))
    .await
    .map(Json::into_inner)
    .map_err(HandlerError::from)
}

/// Builds the workflow report from the operation's persisted counters.
///
/// The counts come from the row rather than from anything the workflow body
/// accumulated in memory, so a resumed execution reports the whole rebuild
/// rather than only the batches this attempt happened to run.
async fn report(
    ctx: &WorkflowContext<'_>,
    repository: &RebuildRepository,
    request: &KnowledgeIndexRebuildRequest,
    generation_uid: Option<EmbeddingGenerationId>,
    lifecycle: RebuildLifecycle,
    validation_overlap: Option<f64>,
) -> Result<KnowledgeIndexRebuildReport, HandlerError> {
    let repository = repository.clone();
    let operation_uid = request.operation_uid;
    let tenant_id = request.tenant_id;
    ctx.run(move || {
        let repository = repository.clone();
        async move {
            let operation = repository
                .load_operation(operation_uid)
                .await
                .map_err(rebuild_error)?
                .ok_or_else(|| TerminalError::new("rebuild operation disappeared"))?;
            Ok::<_, HandlerError>(Json::from(KnowledgeIndexRebuildReport {
                operation_uid,
                tenant_id,
                generation_uid,
                lifecycle,
                vectors_total: operation.vectors_total,
                vectors_rebuilt: operation.vectors_rebuilt,
                vectors_failed: operation.vectors_failed,
                validation_overlap,
                estimated_cost_micros: operation.estimated_cost_micros,
            }))
        }
    })
    .name(format!("rebuild_report_{lifecycle}"))
    .await
    .map(Json::into_inner)
    .map_err(HandlerError::from)
}

fn rebuild_error(error: moa_memory_vector::Error) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_ids_are_keyed_by_operation_so_a_resubmission_resumes() {
        // Pins: submitting the same operation twice attaches to the running
        // workflow instead of starting a second rebuild of the same partition.
        let operation = RebuildOperationId(Uuid::from_u128(7));

        assert_eq!(
            knowledge_index_rebuild_workflow_id(operation),
            "knowledge-index-rebuild:00000000-0000-0000-0000-000000000007"
        );
        assert_ne!(
            knowledge_index_rebuild_workflow_id(operation),
            knowledge_index_rebuild_workflow_id(RebuildOperationId(Uuid::from_u128(8)))
        );
    }
}
