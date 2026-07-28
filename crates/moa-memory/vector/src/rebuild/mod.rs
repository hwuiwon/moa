//! Durable storage-partition index rebuilds.
//!
//! A rebuild recomputes a whole partition's vectors — under a new embedding
//! model, or after a chunking change — while the partition keeps serving from
//! the vectors it already has. The shape that makes that safe:
//!
//! * The candidate generation is built into its own table. Production reads
//!   `moa.embeddings`; candidates live in
//!   `moa.knowledge_rebuild_candidate_vector`. A shadow hit cannot leak into
//!   retrieval, ranking, hydration, lineage, or citations because it is not in
//!   the table those paths read. That is a property of the schema, not of a
//!   predicate someone has to remember.
//! * Every lifecycle transition is a compare-and-swap that also checks the
//!   owner token. A replayed Restate step observes `AlreadyApplied` instead of
//!   re-running; a foreign writer loses the swap and gets a typed error naming
//!   what it actually observed.
//! * Progress is a keyset checkpoint that only moves forward, and
//!   `vectors_rebuilt` is recomputed from the candidate table rather than
//!   incremented. A replayed batch therefore cannot inflate the count, and a
//!   replayed *older* batch cannot rewind the cursor.
//! * Activation is one compare-and-swap on a single pointer row, and it refuses
//!   an incomplete generation. Rollback swaps the pointer back to the retained
//!   prior generation. Finalization is what finally discards retired data.

pub mod source;
pub mod validation;

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::{
    EmbeddingGenerationId, GenerationState, RebuildKind, RebuildLifecycle, RebuildOperationId,
    RlsContext,
};
use moa_db::ScopedConn;
use pgvector::HalfVector;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::{Error, Result, VECTOR_DIMENSION, validate_dimension};
use source::AuthoritativeInput;

/// Number of source vectors reconstructed and embedded per durable batch.
pub const REBUILD_BATCH_SIZE: i64 = 128;

/// Minimum mean top-K overlap a candidate generation must reach to activate.
///
/// The rule is the pure overlap comparison the backend-promotion engine already
/// uses. Nothing else is reused from that engine: its `dual_read` window can
/// serve target-backend hits to production, and this path must never serve a
/// candidate.
pub const REBUILD_OVERLAP_THRESHOLD: f64 = 0.95;

/// One durable rebuild operation.
#[derive(Debug, Clone, PartialEq)]
pub struct RebuildOperation {
    /// Operation identity.
    pub operation_uid: RebuildOperationId,
    /// Tenant that owns the partition.
    pub tenant_id: TenantId,
    /// Storage partition being rebuilt.
    pub storage_partition_id: String,
    /// Which rebuild this is.
    pub kind: RebuildKind,
    /// Current lifecycle position.
    pub lifecycle: RebuildLifecycle,
    /// Workflow execution that owns the operation.
    pub owner_token: Uuid,
    /// Compare-and-swap counter advanced by every accepted transition.
    pub fence_token: i64,
    /// Candidate generation being built, once one exists.
    pub candidate_generation_uid: Option<EmbeddingGenerationId>,
    /// Last source uid whose candidate batch committed.
    pub checkpoint_uid: Option<Uuid>,
    /// Number of committed batches.
    pub checkpoint_batch_index: i64,
    /// Partition-wide census taken at planning time.
    pub vectors_total: i64,
    /// Candidate vectors written so far, recomputed from the candidate table.
    pub vectors_rebuilt: i64,
    /// Source vectors whose input could not be reconstructed.
    pub vectors_failed: i64,
    /// Deterministic input-token estimate.
    pub estimated_input_tokens: i64,
    /// Deterministic cost projection in micros. Never a billed figure.
    pub estimated_cost_micros: i64,
    /// Embedding requests issued.
    pub provider_requests: i64,
    /// Embedding requests the provider throttled.
    pub provider_throttles: i64,
    /// Embedding requests retried after a transient failure.
    pub provider_retries: i64,
    /// Closed-vocabulary failure code, when the operation recorded one.
    pub last_error_code: Option<String>,
    /// Operator-safe failure summary. Never carries provider or document text.
    pub last_error_message: Option<String>,
    /// When an operator asked the operation to stop.
    pub cancel_requested_at: Option<DateTime<Utc>>,
    /// When the candidate generation passed validation.
    pub validated_at: Option<DateTime<Utc>>,
    /// When the candidate generation became the production read generation.
    pub activated_at: Option<DateTime<Utc>>,
    /// When the prior generation was restored.
    pub rolled_back_at: Option<DateTime<Utc>>,
    /// When retired data was discarded.
    pub finalized_at: Option<DateTime<Utc>>,
    /// Creation instant.
    pub created_at: DateTime<Utc>,
    /// Last update instant.
    pub updated_at: DateTime<Utc>,
}

/// One embedding generation of a storage partition.
#[derive(Debug, Clone, PartialEq)]
pub struct RebuildGeneration {
    /// Generation identity.
    pub generation_uid: EmbeddingGenerationId,
    /// Tenant that owns the partition.
    pub tenant_id: TenantId,
    /// Storage partition this generation belongs to.
    pub storage_partition_id: String,
    /// Monotonic sequence within the partition.
    pub generation_seq: i64,
    /// Operation that built it, absent for the adopted bootstrap generation.
    pub operation_uid: Option<RebuildOperationId>,
    /// Embedding model that produced every vector in this generation.
    pub embedding_model: String,
    /// Embedding model version.
    pub embedding_model_version: i32,
    /// Embedding dimensionality.
    pub embedding_dimension: i32,
    /// Generation-specific external namespace.
    pub turbopuffer_namespace: String,
    /// Serving state.
    pub state: GenerationState,
    /// Whether every source vector has a candidate row.
    pub complete: bool,
    /// Candidate or activated vector count.
    pub vector_count: i64,
    /// Mean top-K overlap measured against the previous generation.
    pub validation_overlap: Option<f64>,
    /// Creation instant.
    pub created_at: DateTime<Utc>,
    /// Activation instant.
    pub activated_at: Option<DateTime<Utc>>,
    /// Retirement instant.
    pub retired_at: Option<DateTime<Utc>>,
}

/// The production read generation for one storage partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveGenerationPointer {
    /// Storage partition the pointer describes.
    pub storage_partition_id: String,
    /// Tenant that owns the partition.
    pub tenant_id: TenantId,
    /// Generation production reads serve from.
    pub generation_uid: EmbeddingGenerationId,
    /// Generation a rollback restores, when one is retained.
    pub previous_generation_uid: Option<EmbeddingGenerationId>,
    /// Compare-and-swap counter for the pointer itself.
    pub pointer_version: i64,
}

/// Outcome of a compare-and-swap lifecycle transition.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionOutcome {
    /// The swap moved the operation into the requested lifecycle.
    Applied(Box<RebuildOperation>),
    /// The operation was already in the requested lifecycle under the same
    /// owner. This is what a replayed durable step sees, and it is success.
    AlreadyApplied(Box<RebuildOperation>),
}

impl TransitionOutcome {
    /// Returns the operation row in either outcome.
    #[must_use]
    pub fn operation(&self) -> &RebuildOperation {
        match self {
            Self::Applied(operation) | Self::AlreadyApplied(operation) => operation,
        }
    }

    /// Consumes the outcome and returns the operation row.
    #[must_use]
    pub fn into_operation(self) -> RebuildOperation {
        match self {
            Self::Applied(operation) | Self::AlreadyApplied(operation) => *operation,
        }
    }
}

/// Inputs required to start one rebuild operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRebuild {
    /// Operation identity chosen by the caller so a replay reuses it.
    pub operation_uid: RebuildOperationId,
    /// Owner token identifying the workflow execution.
    pub owner_token: Uuid,
    /// Which rebuild to run.
    pub kind: RebuildKind,
    /// Embedding model the candidate generation will use.
    pub embedding_model: String,
    /// Embedding model version.
    pub embedding_model_version: i32,
    /// Per-million-input-token rate used for the deterministic cost estimate.
    pub estimate_micros_per_million_tokens: i64,
}

/// One candidate vector staged for an unactivated generation.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateVector {
    /// Graph node identity.
    pub uid: Uuid,
    /// Contact owner for contact-scoped rows.
    pub user_id: Option<String>,
    /// Graph vertex label.
    pub label: String,
    /// Sensitivity class carried from the source embedding row.
    pub pii_class: String,
    /// Recomputed embedding.
    pub embedding: Vec<f32>,
    /// SHA-256 of the authoritative input this vector was built from.
    pub input_digest: Vec<u8>,
    /// Deterministic token estimate for the input.
    pub input_token_estimate: i32,
}

impl CandidateVector {
    /// Builds a candidate from a reconstructed input and its recomputed vector.
    #[must_use]
    pub fn from_input(input: &AuthoritativeInput, embedding: Vec<f32>) -> Self {
        Self {
            uid: input.uid,
            user_id: input.user_id.clone(),
            label: input.label.clone(),
            pii_class: input.pii_class.as_str().to_string(),
            embedding,
            input_digest: input.digest(),
            input_token_estimate: input.estimated_tokens(),
        }
    }
}

/// The identity a rebuild writer must present to change durable state.
///
/// Grouped because these three travel together on every write and mean one
/// thing: "this execution, of this operation, building this generation." A call
/// site that had to pass them separately could mismatch the generation against
/// the operation, which is exactly the cross-generation overwrite the
/// compare-and-swap exists to refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildFence {
    /// Operation being advanced.
    pub operation_uid: RebuildOperationId,
    /// Workflow execution that owns the operation.
    pub owner_token: Uuid,
    /// Candidate generation the operation is building.
    pub generation_uid: EmbeddingGenerationId,
}

/// One durable build batch presented for commit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatchCommit<'a> {
    /// Candidate vectors produced by this batch.
    pub candidates: &'a [CandidateVector],
    /// Last source uid this batch covered; the checkpoint advances to it.
    pub checkpoint_uid: Uuid,
    /// Monotonic batch index, recorded for operator visibility.
    pub batch_index: i64,
    /// Counters accumulated while producing the batch.
    pub counters: BatchCounters,
    /// Deterministic per-million-input-token rate for the cost projection.
    pub estimate_micros_per_million_tokens: i64,
}

/// Counters accumulated by one durable build batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchCounters {
    /// Source vectors whose input could not be reconstructed in this batch.
    pub vectors_failed: i64,
    /// Input tokens estimated for this batch.
    pub estimated_input_tokens: i64,
    /// Embedding requests issued for this batch.
    pub provider_requests: i64,
    /// Embedding requests the provider throttled.
    pub provider_throttles: i64,
    /// Embedding requests retried after a transient failure.
    pub provider_retries: i64,
}

/// Durable repository for storage-partition rebuild state.
///
/// Every method opens its own `moa_app` scoped transaction, so row-level
/// security applies to the rebuild exactly as it does to ordinary tenant
/// traffic. A rebuild has no elevated read path into another tenant.
#[derive(Debug, Clone)]
pub struct RebuildRepository {
    pool: PgPool,
    scope: RlsContext,
}

impl RebuildRepository {
    /// Creates a rebuild repository bound to one tenant scope.
    #[must_use]
    pub fn new(pool: PgPool, scope: RlsContext) -> Self {
        Self { pool, scope }
    }

    /// Returns the tenant this repository is scoped to.
    #[must_use]
    pub fn tenant_id(&self) -> TenantId {
        self.scope.tenant_id()
    }

    /// Returns the storage partition this repository is scoped to.
    #[must_use]
    pub fn storage_partition_id(&self) -> String {
        self.scope.storage_partition_id().to_string()
    }

    async fn begin(&self) -> Result<ScopedConn<'_>> {
        Ok(ScopedConn::begin_as_app(&self.pool, &self.scope, true).await?)
    }

    /// Starts a rebuild, refusing a partition that already has a live one.
    ///
    /// The refusal comes from the partial unique index, not from a read-then-write
    /// check: two concurrent starts both see no live operation, and exactly one
    /// INSERT survives.
    pub async fn start_operation(&self, request: StartRebuild) -> Result<RebuildOperation> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;

        // A replayed start finds its own row and returns it unchanged. A
        // different operation id on a busy partition is a genuine conflict.
        if let Some(existing) = load_operation_in(conn.as_mut(), request.operation_uid.0).await? {
            conn.commit().await?;
            return Ok(existing);
        }

        let insert = sqlx::query(
            r#"
            INSERT INTO moa.knowledge_rebuild_operation
                (operation_uid, tenant_id, storage_partition_id, kind, lifecycle, owner_token)
            VALUES ($1, $2, $3, $4, 'planning', $5)
            ON CONFLICT DO NOTHING
            RETURNING operation_uid
            "#,
        )
        .bind(request.operation_uid.0)
        .bind(self.tenant_id().0)
        .bind(&storage_partition_id)
        .bind(request.kind.as_str())
        .bind(request.owner_token)
        .fetch_optional(conn.as_mut())
        .await?;

        if insert.is_none() {
            conn.rollback().await?;
            return Err(Error::RebuildPartitionBusy {
                storage_partition_id,
            });
        }

        let operation = load_operation_in(conn.as_mut(), request.operation_uid.0)
            .await?
            .ok_or(Error::RebuildOperationNotFound {
                operation_uid: request.operation_uid.0,
            })?;
        conn.commit().await?;
        Ok(operation)
    }

    /// Counts the vectors in this partition that a rebuild must reproduce.
    pub async fn count_partition_vectors(&self) -> Result<i64> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        let count = source::count_partition_vectors(conn.as_mut(), &storage_partition_id).await?;
        conn.commit().await?;
        Ok(count)
    }

    /// Returns partition labels no reconstruction rule covers.
    pub async fn unrebuildable_labels(&self) -> Result<Vec<String>> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        let labels = source::unrebuildable_labels(conn.as_mut(), &storage_partition_id).await?;
        conn.commit().await?;
        Ok(labels)
    }

    /// Loads one keyset page of authoritative embedding inputs.
    pub async fn load_authoritative_inputs(
        &self,
        after_uid: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<AuthoritativeInput>> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        let inputs = source::load_authoritative_inputs(
            conn.as_mut(),
            &storage_partition_id,
            after_uid,
            limit,
        )
        .await?;
        conn.commit().await?;
        Ok(inputs)
    }

    /// Scores a candidate generation with bounded shadow queries.
    pub async fn validate_candidate_generation(
        &self,
        generation_uid: EmbeddingGenerationId,
        sample_limit: i64,
    ) -> Result<validation::ShadowValidation> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        let outcome = validation::validate_candidate_generation(
            conn.as_mut(),
            &storage_partition_id,
            generation_uid.0,
            sample_limit,
        )
        .await?;
        conn.commit().await?;
        Ok(outcome)
    }

    /// Loads one rebuild operation.
    pub async fn load_operation(
        &self,
        operation_uid: RebuildOperationId,
    ) -> Result<Option<RebuildOperation>> {
        let mut conn = self.begin().await?;
        let operation = load_operation_in(conn.as_mut(), operation_uid.0).await?;
        conn.commit().await?;
        Ok(operation)
    }

    /// Loads the partition's live operation, if one exists.
    pub async fn load_live_operation(&self) -> Result<Option<RebuildOperation>> {
        let mut conn = self.begin().await?;
        let row = sqlx::query(&format!(
            "{OPERATION_COLUMNS} WHERE storage_partition_id = $1 \
             AND lifecycle NOT IN ('finalized', 'rolled_back', 'cancelled', 'failed')"
        ))
        .bind(self.storage_partition_id())
        .fetch_optional(conn.as_mut())
        .await?;
        conn.commit().await?;
        row.map(decode_operation).transpose()
    }

    /// Moves an operation between lifecycles under owner and lifecycle CAS.
    ///
    /// `from` is the lifecycle the caller believes is current. A replay whose
    /// swap already landed returns `AlreadyApplied` rather than failing, because
    /// the row it finds is the row it intended to write.
    pub async fn transition(
        &self,
        operation_uid: RebuildOperationId,
        owner_token: Uuid,
        from: RebuildLifecycle,
        to: RebuildLifecycle,
    ) -> Result<TransitionOutcome> {
        let mut conn = self.begin().await?;
        let outcome = transition_in(conn.as_mut(), operation_uid.0, owner_token, from, to).await;
        match outcome {
            Ok(outcome) => {
                conn.commit().await?;
                Ok(outcome)
            }
            Err(error) => {
                conn.rollback().await?;
                Err(error)
            }
        }
    }

    /// Records the partition census and the planned cost estimate.
    pub async fn record_plan(
        &self,
        operation_uid: RebuildOperationId,
        owner_token: Uuid,
        vectors_total: i64,
    ) -> Result<RebuildOperation> {
        let mut conn = self.begin().await?;
        let updated = sqlx::query(
            r#"
            UPDATE moa.knowledge_rebuild_operation
               SET vectors_total = $3,
                   fence_token = fence_token + 1,
                   updated_at = now()
             WHERE operation_uid = $1
               AND owner_token = $2
               AND lifecycle = 'planning'
            "#,
        )
        .bind(operation_uid.0)
        .bind(owner_token)
        .bind(vectors_total)
        .execute(conn.as_mut())
        .await?;
        if updated.rows_affected() == 0 {
            // A replay finds the census already recorded; anything else is a
            // foreign writer or a lifecycle that moved on.
            let observed = load_operation_in(conn.as_mut(), operation_uid.0).await?;
            conn.rollback().await?;
            return match observed {
                Some(operation)
                    if operation.owner_token == owner_token
                        && operation.vectors_total == vectors_total =>
                {
                    Ok(operation)
                }
                Some(operation) => Err(Error::RebuildFenceLost {
                    operation_uid: operation_uid.0,
                    expected: RebuildLifecycle::Planning.as_str(),
                    observed: operation.lifecycle.as_str(),
                }),
                None => Err(Error::RebuildOperationNotFound {
                    operation_uid: operation_uid.0,
                }),
            };
        }
        let operation = load_operation_in(conn.as_mut(), operation_uid.0)
            .await?
            .ok_or(Error::RebuildOperationNotFound {
                operation_uid: operation_uid.0,
            })?;
        conn.commit().await?;
        Ok(operation)
    }

    /// Creates the candidate generation this operation will build into.
    ///
    /// Idempotent: a replay finds the operation already pointing at a candidate
    /// generation and returns it, so a retried step cannot leave two candidate
    /// generations racing for one partition.
    pub async fn create_candidate_generation(
        &self,
        operation_uid: RebuildOperationId,
        owner_token: Uuid,
        generation_uid: EmbeddingGenerationId,
        embedding_model: &str,
        embedding_model_version: i32,
        namespace_prefix: &str,
    ) -> Result<RebuildGeneration> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        let operation = load_operation_in(conn.as_mut(), operation_uid.0)
            .await?
            .ok_or(Error::RebuildOperationNotFound {
                operation_uid: operation_uid.0,
            })?;
        if operation.owner_token != owner_token {
            conn.rollback().await?;
            return Err(Error::RebuildFenceLost {
                operation_uid: operation_uid.0,
                expected: "owned candidate generation",
                observed: "operation owned by another execution",
            });
        }
        if let Some(existing) = operation.candidate_generation_uid {
            let generation = load_generation_in(conn.as_mut(), existing.0).await?.ok_or(
                Error::RebuildGenerationNotFound {
                    generation_uid: existing.0,
                },
            )?;
            conn.commit().await?;
            return Ok(generation);
        }

        let next_seq: i64 = sqlx::query_scalar(
            r#"
            SELECT COALESCE(max(generation_seq), 0) + 1
              FROM moa.knowledge_rebuild_generation
             WHERE storage_partition_id = $1
            "#,
        )
        .bind(&storage_partition_id)
        .fetch_one(conn.as_mut())
        .await?;

        let namespace = generation_namespace(namespace_prefix, next_seq)?;
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_rebuild_generation
                (generation_uid, tenant_id, storage_partition_id, generation_seq, operation_uid,
                 embedding_model, embedding_model_version, embedding_dimension,
                 turbopuffer_namespace, state)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'candidate')
            "#,
        )
        .bind(generation_uid.0)
        .bind(self.tenant_id().0)
        .bind(&storage_partition_id)
        .bind(next_seq)
        .bind(operation_uid.0)
        .bind(embedding_model)
        .bind(embedding_model_version)
        .bind(i32::try_from(VECTOR_DIMENSION).unwrap_or(i32::MAX))
        .bind(&namespace)
        .execute(conn.as_mut())
        .await?;

        sqlx::query(
            r#"
            UPDATE moa.knowledge_rebuild_operation
               SET candidate_generation_uid = $3,
                   fence_token = fence_token + 1,
                   updated_at = now()
             WHERE operation_uid = $1
               AND owner_token = $2
            "#,
        )
        .bind(operation_uid.0)
        .bind(owner_token)
        .bind(generation_uid.0)
        .execute(conn.as_mut())
        .await?;

        let generation = load_generation_in(conn.as_mut(), generation_uid.0)
            .await?
            .ok_or(Error::RebuildGenerationNotFound {
                generation_uid: generation_uid.0,
            })?;
        conn.commit().await?;
        Ok(generation)
    }

    /// Adopts the partition's current vectors as its bootstrap generation.
    ///
    /// Partitions that predate rebuilds have vectors but no generation row. The
    /// first rebuild records what is already serving, so activation has a
    /// previous generation to retain and rollback has somewhere to return to.
    pub async fn ensure_bootstrap_generation(
        &self,
        embedding_model: &str,
        embedding_model_version: i32,
        namespace_prefix: &str,
    ) -> Result<ActiveGenerationPointer> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        if let Some(pointer) = load_pointer_in(conn.as_mut(), &storage_partition_id).await? {
            conn.commit().await?;
            return Ok(pointer);
        }

        let generation_uid = EmbeddingGenerationId::new();
        let namespace = generation_namespace(namespace_prefix, 1)?;
        let vector_count =
            source::count_partition_vectors(conn.as_mut(), &storage_partition_id).await?;
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_rebuild_generation
                (generation_uid, tenant_id, storage_partition_id, generation_seq, operation_uid,
                 embedding_model, embedding_model_version, embedding_dimension,
                 turbopuffer_namespace, state, complete, vector_count, activated_at)
            VALUES ($1, $2, $3, 1, NULL, $4, $5, $6, $7, 'active', TRUE, $8, now())
            ON CONFLICT (storage_partition_id, generation_seq) DO NOTHING
            "#,
        )
        .bind(generation_uid.0)
        .bind(self.tenant_id().0)
        .bind(&storage_partition_id)
        .bind(embedding_model)
        .bind(embedding_model_version)
        .bind(i32::try_from(VECTOR_DIMENSION).unwrap_or(i32::MAX))
        .bind(&namespace)
        .bind(vector_count)
        .execute(conn.as_mut())
        .await?;

        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_active_generation
                (storage_partition_id, tenant_id, generation_uid, previous_generation_uid)
            VALUES ($1, $2, $3, NULL)
            ON CONFLICT (storage_partition_id) DO NOTHING
            "#,
        )
        .bind(&storage_partition_id)
        .bind(self.tenant_id().0)
        .bind(generation_uid.0)
        .execute(conn.as_mut())
        .await?;

        let pointer = load_pointer_in(conn.as_mut(), &storage_partition_id)
            .await?
            .ok_or_else(|| Error::ActiveGenerationMissing {
                storage_partition_id: storage_partition_id.clone(),
            })?;
        conn.commit().await?;
        Ok(pointer)
    }

    /// Reads the production read generation for this partition.
    pub async fn load_active_generation(&self) -> Result<Option<ActiveGenerationPointer>> {
        let mut conn = self.begin().await?;
        let pointer = load_pointer_in(conn.as_mut(), &self.storage_partition_id()).await?;
        conn.commit().await?;
        Ok(pointer)
    }

    /// Loads one generation row.
    pub async fn load_generation(
        &self,
        generation_uid: EmbeddingGenerationId,
    ) -> Result<Option<RebuildGeneration>> {
        let mut conn = self.begin().await?;
        let generation = load_generation_in(conn.as_mut(), generation_uid.0).await?;
        conn.commit().await?;
        Ok(generation)
    }

    /// Commits one build batch and advances the durable checkpoint.
    ///
    /// Candidate rows upsert on `(generation_uid, uid)`, so a replayed batch
    /// rewrites the same rows instead of duplicating them. `vectors_rebuilt` is
    /// then recomputed by counting those rows rather than incremented, which is
    /// why a replay cannot inflate it. The checkpoint only moves forward, so a
    /// replayed *earlier* batch cannot rewind progress and cause the build to
    /// redo work it already finished.
    pub async fn commit_batch(
        &self,
        fence: RebuildFence,
        batch: BatchCommit<'_>,
    ) -> Result<RebuildOperation> {
        let RebuildFence {
            operation_uid,
            owner_token,
            generation_uid,
        } = fence;
        let BatchCommit {
            candidates,
            checkpoint_uid,
            batch_index,
            counters,
            estimate_micros_per_million_tokens,
        } = batch;
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;

        let operation = load_operation_in(conn.as_mut(), operation_uid.0)
            .await?
            .ok_or(Error::RebuildOperationNotFound {
                operation_uid: operation_uid.0,
            })?;
        if operation.owner_token != owner_token {
            conn.rollback().await?;
            return Err(Error::RebuildFenceLost {
                operation_uid: operation_uid.0,
                expected: "owned build batch",
                observed: "operation owned by another execution",
            });
        }
        if operation.lifecycle != RebuildLifecycle::Building {
            conn.rollback().await?;
            return Err(Error::RebuildFenceLost {
                operation_uid: operation_uid.0,
                expected: RebuildLifecycle::Building.as_str(),
                observed: operation.lifecycle.as_str(),
            });
        }
        // A candidate written into a generation this operation does not own
        // would let one rebuild overwrite another's vectors.
        if operation.candidate_generation_uid != Some(generation_uid) {
            conn.rollback().await?;
            return Err(Error::RebuildGenerationMismatch {
                operation_uid: operation_uid.0,
                generation_uid: generation_uid.0,
            });
        }

        for candidate in candidates {
            validate_dimension(&candidate.embedding)?;
            let embedding = HalfVector::from_f32_slice(&candidate.embedding);
            sqlx::query(
                r#"
                INSERT INTO moa.knowledge_rebuild_candidate_vector
                    (generation_uid, uid, tenant_id, storage_partition_id, user_id, label,
                     pii_class, embedding, input_digest, input_token_estimate)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                ON CONFLICT (generation_uid, uid) DO UPDATE
                    SET user_id = EXCLUDED.user_id,
                        label = EXCLUDED.label,
                        pii_class = EXCLUDED.pii_class,
                        embedding = EXCLUDED.embedding,
                        input_digest = EXCLUDED.input_digest,
                        input_token_estimate = EXCLUDED.input_token_estimate
                "#,
            )
            .bind(generation_uid.0)
            .bind(candidate.uid)
            .bind(self.tenant_id().0)
            .bind(&storage_partition_id)
            .bind(candidate.user_id.as_deref())
            .bind(&candidate.label)
            .bind(&candidate.pii_class)
            .bind(embedding)
            .bind(&candidate.input_digest)
            .bind(candidate.input_token_estimate)
            .execute(conn.as_mut())
            .await?;
        }

        // Counters and the cursor advance together and only forward, so a
        // replayed batch contributes nothing twice.
        let advanced = sqlx::query(
            r#"
            UPDATE moa.knowledge_rebuild_operation
               SET checkpoint_uid = $3,
                   checkpoint_batch_index = $4,
                   vectors_failed = vectors_failed + $5,
                   estimated_input_tokens = estimated_input_tokens + $6,
                   estimated_cost_micros = estimated_cost_micros
                                           + (($6::BIGINT * $10::BIGINT) / 1000000),
                   provider_requests = provider_requests + $7,
                   provider_throttles = provider_throttles + $8,
                   provider_retries = provider_retries + $9,
                   fence_token = fence_token + 1,
                   updated_at = now()
             WHERE operation_uid = $1
               AND owner_token = $2
               AND (checkpoint_uid IS NULL OR checkpoint_uid < $3)
            "#,
        )
        .bind(operation_uid.0)
        .bind(owner_token)
        .bind(checkpoint_uid)
        .bind(batch_index)
        .bind(counters.vectors_failed)
        .bind(counters.estimated_input_tokens)
        .bind(counters.provider_requests)
        .bind(counters.provider_throttles)
        .bind(counters.provider_retries)
        .bind(estimate_micros_per_million_tokens)
        .execute(conn.as_mut())
        .await?;

        if advanced.rows_affected() > 0 {
            sqlx::query(
                r#"
                UPDATE moa.knowledge_rebuild_operation
                   SET vectors_rebuilt = (
                           SELECT count(*)
                             FROM moa.knowledge_rebuild_candidate_vector
                            WHERE generation_uid = $2
                       )
                 WHERE operation_uid = $1
                "#,
            )
            .bind(operation_uid.0)
            .bind(generation_uid.0)
            .execute(conn.as_mut())
            .await?;

            sqlx::query(
                r#"
                UPDATE moa.knowledge_rebuild_generation
                   SET vector_count = (
                           SELECT count(*)
                             FROM moa.knowledge_rebuild_candidate_vector
                            WHERE generation_uid = $1
                       )
                 WHERE generation_uid = $1
                "#,
            )
            .bind(generation_uid.0)
            .execute(conn.as_mut())
            .await?;
        }

        let operation = load_operation_in(conn.as_mut(), operation_uid.0)
            .await?
            .ok_or(Error::RebuildOperationNotFound {
                operation_uid: operation_uid.0,
            })?;
        conn.commit().await?;
        Ok(operation)
    }

    /// Marks a candidate generation complete once every source vector has one.
    ///
    /// Refuses when the candidate count falls short of the census. An
    /// incomplete generation that activated would leave the partition partly
    /// unsearchable, which is worse than the old vectors it replaced.
    pub async fn mark_generation_complete(
        &self,
        generation_uid: EmbeddingGenerationId,
        expected_vectors: i64,
    ) -> Result<RebuildGeneration> {
        let mut conn = self.begin().await?;
        let actual: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.knowledge_rebuild_candidate_vector WHERE generation_uid = $1",
        )
        .bind(generation_uid.0)
        .fetch_one(conn.as_mut())
        .await?;
        if actual < expected_vectors {
            conn.rollback().await?;
            return Err(Error::RebuildGenerationIncomplete {
                generation_uid: generation_uid.0,
                expected: expected_vectors,
                actual,
            });
        }
        sqlx::query(
            r#"
            UPDATE moa.knowledge_rebuild_generation
               SET complete = TRUE,
                   vector_count = $2
             WHERE generation_uid = $1
               AND state = 'candidate'
            "#,
        )
        .bind(generation_uid.0)
        .bind(actual)
        .execute(conn.as_mut())
        .await?;
        let generation = load_generation_in(conn.as_mut(), generation_uid.0)
            .await?
            .ok_or(Error::RebuildGenerationNotFound {
                generation_uid: generation_uid.0,
            })?;
        conn.commit().await?;
        Ok(generation)
    }

    /// Records a validation overlap against the generation.
    pub async fn record_validation(
        &self,
        generation_uid: EmbeddingGenerationId,
        overlap: f64,
    ) -> Result<RebuildGeneration> {
        let mut conn = self.begin().await?;
        sqlx::query(
            "UPDATE moa.knowledge_rebuild_generation SET validation_overlap = $2 WHERE generation_uid = $1",
        )
        .bind(generation_uid.0)
        .bind(overlap.clamp(0.0, 1.0))
        .execute(conn.as_mut())
        .await?;
        let generation = load_generation_in(conn.as_mut(), generation_uid.0)
            .await?
            .ok_or(Error::RebuildGenerationNotFound {
                generation_uid: generation_uid.0,
            })?;
        conn.commit().await?;
        Ok(generation)
    }

    /// Promotes a complete candidate generation to the production read generation.
    ///
    /// One scoped transaction does all of it: candidate vectors move into
    /// `moa.embeddings`, the old generation retires, and the pointer swaps under
    /// compare-and-swap on `pointer_version`. Either the partition serves
    /// entirely from the new generation or entirely from the old one; there is
    /// no instant where half of each is visible.
    pub async fn activate_generation(
        &self,
        generation_uid: EmbeddingGenerationId,
        expected_pointer_version: i64,
    ) -> Result<ActiveGenerationPointer> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        let activated = activate_generation_in_conn(
            conn.as_mut(),
            &storage_partition_id,
            generation_uid,
            expected_pointer_version,
        )
        .await;
        match activated {
            Ok(pointer) => {
                conn.commit().await?;
                Ok(pointer)
            }
            Err(error) => {
                conn.rollback().await?;
                Err(error)
            }
        }
    }

    /// Restores the retained prior generation as the production read generation.
    pub async fn rollback_generation(
        &self,
        expected_pointer_version: i64,
    ) -> Result<ActiveGenerationPointer> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        let pointer = load_pointer_in(conn.as_mut(), &storage_partition_id)
            .await?
            .ok_or_else(|| Error::ActiveGenerationMissing {
                storage_partition_id: storage_partition_id.clone(),
            })?;
        let Some(previous) = pointer.previous_generation_uid else {
            conn.rollback().await?;
            return Err(Error::RebuildRollbackUnavailable {
                storage_partition_id,
            });
        };
        let previous_generation = load_generation_in(conn.as_mut(), previous.0).await?.ok_or(
            Error::RebuildGenerationNotFound {
                generation_uid: previous.0,
            },
        )?;

        let swapped = sqlx::query(
            r#"
            UPDATE moa.knowledge_active_generation
               SET generation_uid = previous_generation_uid,
                   previous_generation_uid = NULL,
                   pointer_version = pointer_version + 1,
                   updated_at = now()
             WHERE storage_partition_id = $1
               AND pointer_version = $2
               AND previous_generation_uid IS NOT NULL
            "#,
        )
        .bind(&storage_partition_id)
        .bind(expected_pointer_version)
        .execute(conn.as_mut())
        .await?;
        if swapped.rows_affected() == 0 {
            conn.rollback().await?;
            return Err(Error::ActiveGenerationPointerConflict {
                storage_partition_id,
                expected: expected_pointer_version,
                observed: pointer.pointer_version,
            });
        }

        sqlx::query(
            r#"
            UPDATE moa.knowledge_rebuild_generation
               SET state = 'retired',
                   retired_at = now()
             WHERE storage_partition_id = $1
               AND state = 'active'
            "#,
        )
        .bind(&storage_partition_id)
        .execute(conn.as_mut())
        .await?;
        sqlx::query(
            r#"
            UPDATE moa.knowledge_rebuild_generation
               SET state = 'active',
                   retired_at = NULL
             WHERE generation_uid = $1
            "#,
        )
        .bind(previous.0)
        .execute(conn.as_mut())
        .await?;

        // The rolled-back-to generation's vectors are restored from its own
        // candidate rows when it has them. The bootstrap generation has none:
        // its vectors were never removed, because activation replaced them only
        // after the pointer swap that this rollback has now undone.
        let restored = promote_candidate_vectors(
            conn.as_mut(),
            &storage_partition_id,
            previous.0,
            &previous_generation.embedding_model,
            previous_generation.embedding_model_version,
        )
        .await?;
        tracing::info!(
            storage_partition_id = %storage_partition_id,
            generation_uid = %previous.0,
            restored,
            "rebuild rollback restored the previous generation"
        );

        let pointer = load_pointer_in(conn.as_mut(), &storage_partition_id)
            .await?
            .ok_or_else(|| Error::ActiveGenerationMissing {
                storage_partition_id: storage_partition_id.clone(),
            })?;
        conn.commit().await?;
        Ok(pointer)
    }

    /// Discards retired generation data once rollback is no longer wanted.
    ///
    /// After this returns, no reader can reconstruct the retired vectors: the
    /// candidate rows are gone and the pointer no longer names a previous
    /// generation to return to.
    pub async fn finalize_generation(&self, generation_uid: EmbeddingGenerationId) -> Result<u64> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        sqlx::query(
            "UPDATE moa.knowledge_active_generation SET previous_generation_uid = NULL, \
             pointer_version = pointer_version + 1, updated_at = now() \
             WHERE storage_partition_id = $1 AND generation_uid = $2",
        )
        .bind(&storage_partition_id)
        .bind(generation_uid.0)
        .execute(conn.as_mut())
        .await?;
        let removed = sqlx::query(
            r#"
            DELETE FROM moa.knowledge_rebuild_candidate_vector
             WHERE storage_partition_id = $1
               AND generation_uid <> $2
            "#,
        )
        .bind(&storage_partition_id)
        .bind(generation_uid.0)
        .execute(conn.as_mut())
        .await?;
        // Staged rechunk state for retired generations goes before the
        // generations themselves: the staging foreign key has no cascade, so a
        // surviving staged row would block the delete rather than vanish with it.
        sqlx::query(
            r#"
            DELETE FROM moa.knowledge_rechunk_staging AS staging
             USING moa.knowledge_rebuild_generation AS generation
             WHERE staging.generation_uid = generation.generation_uid
               AND generation.storage_partition_id = $1
               AND generation.state = 'retired'
            "#,
        )
        .bind(&storage_partition_id)
        .execute(conn.as_mut())
        .await?;
        sqlx::query(
            r#"
            DELETE FROM moa.knowledge_rebuild_generation
             WHERE storage_partition_id = $1
               AND state = 'retired'
            "#,
        )
        .bind(&storage_partition_id)
        .execute(conn.as_mut())
        .await?;
        conn.commit().await?;
        Ok(removed.rows_affected())
    }

    /// Records an operator cancellation request.
    ///
    /// Cancellation is cooperative: the build checks for it between batches and
    /// stops at a committed checkpoint rather than being interrupted mid-batch.
    pub async fn request_cancel(&self, operation_uid: RebuildOperationId) -> Result<bool> {
        let mut conn = self.begin().await?;
        let updated = sqlx::query(
            r#"
            UPDATE moa.knowledge_rebuild_operation
               SET cancel_requested_at = COALESCE(cancel_requested_at, now()),
                   updated_at = now()
             WHERE operation_uid = $1
               AND lifecycle NOT IN ('finalized', 'rolled_back', 'cancelled', 'failed')
            "#,
        )
        .bind(operation_uid.0)
        .execute(conn.as_mut())
        .await?;
        conn.commit().await?;
        Ok(updated.rows_affected() > 0)
    }

    /// Records an operator-safe failure summary on the operation.
    pub async fn record_error(
        &self,
        operation_uid: RebuildOperationId,
        error_code: &str,
        message: &str,
    ) -> Result<()> {
        let mut conn = self.begin().await?;
        sqlx::query(
            r#"
            UPDATE moa.knowledge_rebuild_operation
               SET last_error_code = $2,
                   last_error_message = $3,
                   updated_at = now()
             WHERE operation_uid = $1
            "#,
        )
        .bind(operation_uid.0)
        .bind(error_code)
        .bind(safe_error_message(message))
        .execute(conn.as_mut())
        .await?;
        conn.commit().await?;
        Ok(())
    }
}

/// Promotes a complete candidate generation inside the caller's transaction.
///
/// Exposed as an in-connection operation because rechunk activation must
/// replace chunk, graph, vector, changelog, and outbox state *and* flip the
/// generation pointer at one boundary. Duplicating the pointer compare-and-swap
/// in the knowledge crate would create a second definition of atomicity that
/// could drift from this one; instead both callers run these exact statements.
///
/// The caller owns the transaction and therefore owns the commit or rollback.
pub async fn activate_generation_in_conn(
    conn: &mut PgConnection,
    storage_partition_id: &str,
    generation_uid: EmbeddingGenerationId,
    expected_pointer_version: i64,
) -> Result<ActiveGenerationPointer> {
    let generation = load_generation_in(&mut *conn, generation_uid.0)
        .await?
        .ok_or(Error::RebuildGenerationNotFound {
            generation_uid: generation_uid.0,
        })?;
    if !generation.complete {
        return Err(Error::RebuildGenerationIncomplete {
            generation_uid: generation_uid.0,
            expected: generation.vector_count,
            actual: generation.vector_count,
        });
    }
    if generation.state != GenerationState::Candidate {
        return Err(Error::RebuildGenerationNotActivatable {
            generation_uid: generation_uid.0,
            state: generation.state.as_str(),
        });
    }

    let pointer = load_pointer_in(&mut *conn, storage_partition_id)
        .await?
        .ok_or_else(|| Error::ActiveGenerationMissing {
            storage_partition_id: storage_partition_id.to_string(),
        })?;

    let swapped = sqlx::query(
        r#"
        UPDATE moa.knowledge_active_generation
           SET generation_uid = $3,
               previous_generation_uid = generation_uid,
               pointer_version = pointer_version + 1,
               updated_at = now()
         WHERE storage_partition_id = $1
           AND pointer_version = $2
        "#,
    )
    .bind(storage_partition_id)
    .bind(expected_pointer_version)
    .bind(generation_uid.0)
    .execute(&mut *conn)
    .await?;
    if swapped.rows_affected() == 0 {
        return Err(Error::ActiveGenerationPointerConflict {
            storage_partition_id: storage_partition_id.to_string(),
            expected: expected_pointer_version,
            observed: pointer.pointer_version,
        });
    }

    // Retire before promoting so the single-active partial index never sees two
    // rows claiming 'active' inside the statement sequence.
    sqlx::query(
        r#"
        UPDATE moa.knowledge_rebuild_generation
           SET state = 'retired',
               retired_at = now()
         WHERE storage_partition_id = $1
           AND state = 'active'
        "#,
    )
    .bind(storage_partition_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        r#"
        UPDATE moa.knowledge_rebuild_generation
           SET state = 'active',
               activated_at = now()
         WHERE generation_uid = $1
        "#,
    )
    .bind(generation_uid.0)
    .execute(&mut *conn)
    .await?;

    promote_candidate_vectors(
        &mut *conn,
        storage_partition_id,
        generation_uid.0,
        &generation.embedding_model,
        generation.embedding_model_version,
    )
    .await?;

    load_pointer_in(&mut *conn, storage_partition_id)
        .await?
        .ok_or_else(|| Error::ActiveGenerationMissing {
            storage_partition_id: storage_partition_id.to_string(),
        })
}

/// Copies an activated generation's candidate vectors into the served table.
///
/// Runs inside the activation transaction. Rows the generation does not contain
/// are removed so a rebuild that dropped a node does not leave its vector
/// behind under the new generation's model.
async fn promote_candidate_vectors(
    conn: &mut PgConnection,
    storage_partition_id: &str,
    generation_uid: Uuid,
    embedding_model: &str,
    embedding_model_version: i32,
) -> Result<u64> {
    let staged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.knowledge_rebuild_candidate_vector WHERE generation_uid = $1",
    )
    .bind(generation_uid)
    .fetch_one(&mut *conn)
    .await?;
    if staged == 0 {
        return Ok(0);
    }

    let promoted = sqlx::query(
        r#"
        INSERT INTO moa.embeddings
            (uid, storage_partition_id, user_id, label, pii_class, embedding,
             embedding_model, embedding_model_version, valid_to)
        SELECT candidate.uid,
               candidate.storage_partition_id,
               candidate.user_id,
               candidate.label,
               candidate.pii_class,
               candidate.embedding,
               $2,
               $3,
               NULL
          FROM moa.knowledge_rebuild_candidate_vector AS candidate
         WHERE candidate.generation_uid = $1
        ON CONFLICT (storage_partition_id, uid) DO UPDATE
            SET user_id = EXCLUDED.user_id,
                label = EXCLUDED.label,
                pii_class = EXCLUDED.pii_class,
                embedding = EXCLUDED.embedding,
                embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                valid_to = NULL
        "#,
    )
    .bind(generation_uid)
    .bind(embedding_model)
    .bind(embedding_model_version)
    .execute(&mut *conn)
    .await?;

    let orphaned: Vec<Uuid> = sqlx::query_scalar(
        r#"
        DELETE FROM moa.embeddings AS embedding
         WHERE embedding.storage_partition_id = $1
           AND NOT EXISTS (
               SELECT 1
                 FROM moa.knowledge_rebuild_candidate_vector AS candidate
                WHERE candidate.generation_uid = $2
                  AND candidate.uid = embedding.uid
           )
        RETURNING embedding.uid
        "#,
    )
    .bind(storage_partition_id)
    .bind(generation_uid)
    .fetch_all(&mut *conn)
    .await?;

    // External backends learn about the flip from the same transaction that
    // performs it. Enqueueing here rather than after the commit is what keeps
    // the outbox inside the atomic activation boundary: a rolled-back
    // activation leaves no queued remote write describing vectors that never
    // became authoritative. The enqueue is a no-op for pgvector-only
    // partitions, which is the common case.
    let promoted_uids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT uid FROM moa.knowledge_rebuild_candidate_vector WHERE generation_uid = $1",
    )
    .bind(generation_uid)
    .fetch_all(&mut *conn)
    .await?;
    crate::sync::enqueue_external_vector_sync(
        &mut *conn,
        storage_partition_id,
        crate::VectorSyncOperation::Upsert,
        &promoted_uids,
    )
    .await?;
    crate::sync::enqueue_external_vector_sync(
        &mut *conn,
        storage_partition_id,
        crate::VectorSyncOperation::Delete,
        &orphaned,
    )
    .await?;

    Ok(promoted.rows_affected())
}

/// Maximum Turbopuffer namespace length, mirrored from the store's own check.
const MAX_NAMESPACE_BYTES: usize = 128;

/// Returns the generation-specific external namespace.
///
/// Persisted on the generation row rather than recomputed at call sites, so a
/// candidate generation and the generation it replaces can never write into the
/// same external namespace.
///
/// The suffix has to fit: Turbopuffer caps a namespace at 128 bytes, and the
/// base name is already `moa-{env}-{partition-uuid}`. A rebuild that silently
/// produced an over-long name would fail at the first remote write, mid-build,
/// after paying for the embeddings — so the length is checked when the
/// generation is created instead. `_` is inside Turbopuffer's accepted
/// namespace charset, so the separator itself needs no escaping.
pub fn generation_namespace(prefix: &str, generation_seq: i64) -> Result<String> {
    let namespace = format!("{prefix}__g{generation_seq}");
    if namespace.len() > MAX_NAMESPACE_BYTES {
        return Err(Error::TurbopufferConfig(format!(
            "generation namespace `{namespace}` is {} bytes, over Turbopuffer's {MAX_NAMESPACE_BYTES}-byte limit",
            namespace.len()
        )));
    }
    Ok(namespace)
}

/// Truncates a failure summary to the column's safe bound.
fn safe_error_message(message: &str) -> String {
    const MAX: usize = 512;
    if message.len() <= MAX {
        return message.to_string();
    }
    let mut boundary = MAX;
    while boundary > 0 && !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message[..boundary].to_string()
}

const OPERATION_COLUMNS: &str = r#"
SELECT operation_uid, tenant_id, storage_partition_id, kind, lifecycle, owner_token, fence_token,
       candidate_generation_uid, checkpoint_uid, checkpoint_batch_index, vectors_total,
       vectors_rebuilt, vectors_failed, estimated_input_tokens, estimated_cost_micros,
       provider_requests, provider_throttles, provider_retries, last_error_code,
       last_error_message, cancel_requested_at, validated_at, activated_at, rolled_back_at,
       finalized_at, created_at, updated_at
  FROM moa.knowledge_rebuild_operation
"#;

const GENERATION_COLUMNS: &str = r#"
SELECT generation_uid, tenant_id, storage_partition_id, generation_seq, operation_uid,
       embedding_model, embedding_model_version, embedding_dimension, turbopuffer_namespace,
       state, complete, vector_count, validation_overlap, created_at, activated_at, retired_at
  FROM moa.knowledge_rebuild_generation
"#;

async fn load_operation_in(
    conn: &mut PgConnection,
    operation_uid: Uuid,
) -> Result<Option<RebuildOperation>> {
    let row = sqlx::query(&format!("{OPERATION_COLUMNS} WHERE operation_uid = $1"))
        .bind(operation_uid)
        .fetch_optional(conn)
        .await?;
    row.map(decode_operation).transpose()
}

async fn load_generation_in(
    conn: &mut PgConnection,
    generation_uid: Uuid,
) -> Result<Option<RebuildGeneration>> {
    let row = sqlx::query(&format!("{GENERATION_COLUMNS} WHERE generation_uid = $1"))
        .bind(generation_uid)
        .fetch_optional(conn)
        .await?;
    row.map(decode_generation).transpose()
}

async fn load_pointer_in(
    conn: &mut PgConnection,
    storage_partition_id: &str,
) -> Result<Option<ActiveGenerationPointer>> {
    let row = sqlx::query(
        r#"
        SELECT storage_partition_id, tenant_id, generation_uid, previous_generation_uid,
               pointer_version
          FROM moa.knowledge_active_generation
         WHERE storage_partition_id = $1
        "#,
    )
    .bind(storage_partition_id)
    .fetch_optional(conn)
    .await?;
    row.map(|row| {
        Ok(ActiveGenerationPointer {
            storage_partition_id: row.try_get("storage_partition_id")?,
            tenant_id: TenantId::from(row.try_get::<Uuid, _>("tenant_id")?),
            generation_uid: EmbeddingGenerationId(row.try_get("generation_uid")?),
            previous_generation_uid: row
                .try_get::<Option<Uuid>, _>("previous_generation_uid")?
                .map(EmbeddingGenerationId),
            pointer_version: row.try_get("pointer_version")?,
        })
    })
    .transpose()
}

async fn transition_in(
    conn: &mut PgConnection,
    operation_uid: Uuid,
    owner_token: Uuid,
    from: RebuildLifecycle,
    to: RebuildLifecycle,
) -> Result<TransitionOutcome> {
    let updated = sqlx::query(
        r#"
        UPDATE moa.knowledge_rebuild_operation
           SET lifecycle = $4,
               fence_token = fence_token + 1,
               validated_at = CASE WHEN $4 = 'awaiting_activation'
                                   THEN COALESCE(validated_at, now()) ELSE validated_at END,
               activated_at = CASE WHEN $4 = 'activated'
                                   THEN COALESCE(activated_at, now()) ELSE activated_at END,
               rolled_back_at = CASE WHEN $4 = 'rolled_back'
                                     THEN COALESCE(rolled_back_at, now()) ELSE rolled_back_at END,
               finalized_at = CASE WHEN $4 = 'finalized'
                                   THEN COALESCE(finalized_at, now()) ELSE finalized_at END,
               updated_at = now()
         WHERE operation_uid = $1
           AND owner_token = $2
           AND lifecycle = $3
        "#,
    )
    .bind(operation_uid)
    .bind(owner_token)
    .bind(from.as_str())
    .bind(to.as_str())
    .execute(&mut *conn)
    .await?;

    let operation = load_operation_in(&mut *conn, operation_uid)
        .await?
        .ok_or(Error::RebuildOperationNotFound { operation_uid })?;

    if updated.rows_affected() > 0 {
        return Ok(TransitionOutcome::Applied(Box::new(operation)));
    }
    if operation.owner_token == owner_token && operation.lifecycle == to {
        return Ok(TransitionOutcome::AlreadyApplied(Box::new(operation)));
    }
    Err(Error::RebuildFenceLost {
        operation_uid,
        expected: from.as_str(),
        observed: operation.lifecycle.as_str(),
    })
}

fn decode_operation(row: sqlx::postgres::PgRow) -> Result<RebuildOperation> {
    let kind: String = row.try_get("kind")?;
    let lifecycle: String = row.try_get("lifecycle")?;
    Ok(RebuildOperation {
        operation_uid: RebuildOperationId(row.try_get("operation_uid")?),
        tenant_id: TenantId::from(row.try_get::<Uuid, _>("tenant_id")?),
        storage_partition_id: row.try_get("storage_partition_id")?,
        kind: RebuildKind::parse(&kind)?,
        lifecycle: RebuildLifecycle::parse(&lifecycle)?,
        owner_token: row.try_get("owner_token")?,
        fence_token: row.try_get("fence_token")?,
        candidate_generation_uid: row
            .try_get::<Option<Uuid>, _>("candidate_generation_uid")?
            .map(EmbeddingGenerationId),
        checkpoint_uid: row.try_get("checkpoint_uid")?,
        checkpoint_batch_index: row.try_get("checkpoint_batch_index")?,
        vectors_total: row.try_get("vectors_total")?,
        vectors_rebuilt: row.try_get("vectors_rebuilt")?,
        vectors_failed: row.try_get("vectors_failed")?,
        estimated_input_tokens: row.try_get("estimated_input_tokens")?,
        estimated_cost_micros: row.try_get("estimated_cost_micros")?,
        provider_requests: row.try_get("provider_requests")?,
        provider_throttles: row.try_get("provider_throttles")?,
        provider_retries: row.try_get("provider_retries")?,
        last_error_code: row.try_get("last_error_code")?,
        last_error_message: row.try_get("last_error_message")?,
        cancel_requested_at: row.try_get("cancel_requested_at")?,
        validated_at: row.try_get("validated_at")?,
        activated_at: row.try_get("activated_at")?,
        rolled_back_at: row.try_get("rolled_back_at")?,
        finalized_at: row.try_get("finalized_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn decode_generation(row: sqlx::postgres::PgRow) -> Result<RebuildGeneration> {
    let state: String = row.try_get("state")?;
    Ok(RebuildGeneration {
        generation_uid: EmbeddingGenerationId(row.try_get("generation_uid")?),
        tenant_id: TenantId::from(row.try_get::<Uuid, _>("tenant_id")?),
        storage_partition_id: row.try_get("storage_partition_id")?,
        generation_seq: row.try_get("generation_seq")?,
        operation_uid: row
            .try_get::<Option<Uuid>, _>("operation_uid")?
            .map(RebuildOperationId),
        embedding_model: row.try_get("embedding_model")?,
        embedding_model_version: row.try_get("embedding_model_version")?,
        embedding_dimension: row.try_get("embedding_dimension")?,
        turbopuffer_namespace: row.try_get("turbopuffer_namespace")?,
        state: GenerationState::parse(&state)?,
        complete: row.try_get("complete")?,
        vector_count: row.try_get("vector_count")?,
        validation_overlap: row.try_get("validation_overlap")?,
        created_at: row.try_get("created_at")?,
        activated_at: row.try_get("activated_at")?,
        retired_at: row.try_get("retired_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_namespaces_are_distinct_per_generation() {
        // Pins: two generations of one partition never share an external
        // namespace, so a candidate build cannot overwrite the vectors the
        // active generation is still serving.
        assert_eq!(
            generation_namespace("moa-dev-tenant", 1).expect("within the limit"),
            "moa-dev-tenant__g1"
        );
        assert_ne!(
            generation_namespace("moa-dev-tenant", 1).expect("within the limit"),
            generation_namespace("moa-dev-tenant", 2).expect("within the limit")
        );
    }

    #[test]
    fn the_generation_suffix_fits_the_longest_realistic_namespace() {
        // Pins: the suffix fits Turbopuffer's 128-byte cap on the longest name
        // the store actually derives — `moa-{env}-{vector-type}-{uuid}` — with
        // room for a four-digit generation. The failure this prevents is a
        // rebuild that embeds a whole partition and only then discovers its
        // namespace is unwritable.
        let longest = format!("moa-{}-f16-{}", "production-eu-west-1", uuid::Uuid::nil());
        let namespace =
            generation_namespace(&longest, 9999).expect("the realistic worst case must fit");
        assert!(
            namespace.len() <= MAX_NAMESPACE_BYTES,
            "namespace is {} bytes: {namespace}",
            namespace.len()
        );

        // And an over-long base is refused up front rather than at write time.
        let error = generation_namespace(&"x".repeat(MAX_NAMESPACE_BYTES), 1)
            .expect_err("an over-long namespace must be refused when the generation is created");
        assert!(matches!(error, Error::TurbopufferConfig(_)));
    }

    #[test]
    fn safe_error_messages_stay_within_the_column_bound() {
        // Pins: the operator-visible failure summary is bounded and never
        // truncated mid-character, so an oversized provider message is clipped
        // rather than rejected by the CHECK constraint at write time.
        let long = "é".repeat(400);
        let clipped = safe_error_message(&long);

        assert!(clipped.len() <= 512);
        assert!(long.starts_with(&clipped));
        assert_eq!(safe_error_message("short"), "short");
    }

    #[test]
    fn transition_outcome_exposes_the_row_for_both_replay_and_first_application() {
        // Pins: a replayed durable step and a first application both yield the
        // operation, so workflow code does not branch on which one happened.
        let operation = sample_operation();
        let applied = TransitionOutcome::Applied(Box::new(operation.clone()));
        let replayed = TransitionOutcome::AlreadyApplied(Box::new(operation.clone()));

        assert_eq!(applied.operation(), &operation);
        assert_eq!(replayed.into_operation(), operation);
    }

    fn sample_operation() -> RebuildOperation {
        RebuildOperation {
            operation_uid: RebuildOperationId(Uuid::nil()),
            tenant_id: TenantId::from(Uuid::nil()),
            storage_partition_id: "partition".to_string(),
            kind: RebuildKind::Reembed,
            lifecycle: RebuildLifecycle::Building,
            owner_token: Uuid::nil(),
            fence_token: 1,
            candidate_generation_uid: None,
            checkpoint_uid: None,
            checkpoint_batch_index: 0,
            vectors_total: 0,
            vectors_rebuilt: 0,
            vectors_failed: 0,
            estimated_input_tokens: 0,
            estimated_cost_micros: 0,
            provider_requests: 0,
            provider_throttles: 0,
            provider_retries: 0,
            last_error_code: None,
            last_error_message: None,
            cancel_requested_at: None,
            validated_at: None,
            activated_at: None,
            rolled_back_at: None,
            finalized_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}
