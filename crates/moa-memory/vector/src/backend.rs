//! Storage-partition vector-backend selection.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use moa_core::{MoaConfig, RlsContext};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    Error, PgvectorStore, Result, TurbopufferStore, VectorItem, VectorQuery, VectorStore,
    VectorSyncOperation, VectorSyncReport,
    sync::{
        VECTOR_SYNC_POST_COMMIT_LIMIT, VectorSyncJob, claim_pending_vector_sync,
        enqueue_external_vector_sync, fetch_current_vector_items, mark_vector_sync_failed_batch,
        mark_vector_sync_processed_batch, redrive_dead_lettered_vector_sync,
    },
};

/// Configured vector backend registry for graph-memory call sites.
///
/// The registry owns external backend clients and centralizes the policy for
/// when a caller needs the configured backend versus pgvector as the
/// transactional Postgres source. New vector backends should be added here
/// instead of constructing them from ingestion, lifecycle, or retrieval code.
#[derive(Clone, Default)]
pub struct VectorStoreFactory {
    turbopuffer: Option<Arc<TurbopufferStore>>,
    /// Matryoshka shortlist width applied to every pgvector source this factory builds.
    mrl_shortlist_dims: Option<usize>,
}

impl VectorStoreFactory {
    /// Builds a vector store factory from shared MOA configuration.
    #[must_use]
    pub fn from_config(config: &MoaConfig) -> Self {
        // Ignore an out-of-range shortlist width instead of failing construction:
        // the store's `with_mrl_shortlist` applies the same guard, and a truncated
        // prefix must be strictly shorter than the stored embedding to be meaningful.
        let mrl_shortlist_dims = config
            .memory
            .vector
            .mrl_shortlist_dims
            .filter(|&dims| dims > 0 && dims < crate::VECTOR_DIMENSION);
        if config.memory.vector.mrl_shortlist_dims.is_some() && mrl_shortlist_dims.is_none() {
            tracing::warn!(
                configured = config.memory.vector.mrl_shortlist_dims,
                max = crate::VECTOR_DIMENSION,
                "ignoring memory.vector.mrl_shortlist_dims outside (0, VECTOR_DIMENSION)"
            );
        }
        Self {
            turbopuffer: TurbopufferStore::from_config(config).ok().map(Arc::new),
            mrl_shortlist_dims,
        }
    }

    /// Returns the pgvector source store for normal application-role callers.
    #[must_use]
    pub fn pgvector_source(&self, pool: PgPool, scope: RlsContext) -> Arc<PgvectorStore> {
        Arc::new(PgvectorStore::new(pool, scope).with_mrl_shortlist(self.mrl_shortlist_dims))
    }

    /// Returns the pgvector source store while assuming `moa_app` inside each transaction.
    #[must_use]
    pub fn pgvector_source_for_app_role(
        &self,
        pool: PgPool,
        scope: RlsContext,
    ) -> Arc<PgvectorStore> {
        Arc::new(
            PgvectorStore::new_for_app_role(pool, scope)
                .with_mrl_shortlist(self.mrl_shortlist_dims),
        )
    }

    /// Returns the pgvector source store for tenant control-plane validation reads.
    #[must_use]
    pub fn pgvector_source_for_control_plane(
        &self,
        pool: PgPool,
        scope: RlsContext,
    ) -> Arc<PgvectorStore> {
        Arc::new(
            PgvectorStore::new_for_control_plane(pool, scope)
                .with_mrl_shortlist(self.mrl_shortlist_dims),
        )
    }

    /// Returns the vector store used by transactional graph writes.
    ///
    /// External vector backends cannot participate in the Postgres transaction
    /// driven by `PostgresGraphStore::with_vector_store`. This store writes
    /// pgvector as the transactional source and queues post-commit sync work
    /// for storage partitions configured for an external backend.
    #[must_use]
    pub fn transactional_graph_backend(
        &self,
        pool: PgPool,
        scope: RlsContext,
        assume_app_role: bool,
    ) -> TransactionalGraphVectorBackend {
        let storage_partition_id = scope.storage_partition_id().to_string();
        let source = if assume_app_role {
            self.pgvector_source_for_app_role(pool.clone(), scope)
        } else {
            self.pgvector_source(pool.clone(), scope)
        };
        TransactionalGraphVectorBackend {
            store: Arc::new(TransactionalGraphVectorStore {
                source,
                pool,
                factory: self.clone(),
                storage_partition_id,
                post_commit_limit: VECTOR_SYNC_POST_COMMIT_LIMIT,
            }),
        }
    }

    /// Selects the configured vector store for non-transactional reads or writes.
    ///
    /// Missing `storage_partition_state` rows default to pgvector. Storage
    /// partitions explicitly configured for Turbopuffer require a configured
    /// Turbopuffer client.
    pub async fn configured_for_scope(
        &self,
        pool: &PgPool,
        scope: RlsContext,
        assume_app_role: bool,
    ) -> Result<Arc<dyn VectorStore>> {
        let storage_partition_id = scope.storage_partition_id();
        let pgvector = if assume_app_role {
            self.pgvector_source_for_app_role(pool.clone(), scope)
        } else {
            self.pgvector_source(pool.clone(), scope)
        };
        vector_store_for_storage_partition(
            storage_partition_id.as_str(),
            pool,
            pgvector,
            self.turbopuffer.clone(),
        )
        .await
    }

    /// Drains committed vector-sync outbox rows into configured external backends.
    ///
    /// The drain claims rows with a short lease, applies each operation outside
    /// the graph transaction, and leaves failed rows pending for retry. Partitions
    /// that still use pgvector are marked processed because pgvector is already
    /// the committed source.
    pub async fn drain_external_sync(&self, pool: &PgPool, limit: i64) -> Result<VectorSyncReport> {
        self.drain_external_sync_for_storage_partition(pool, None, limit)
            .await
    }

    /// Re-queues quarantined (dead-lettered) vector-sync rows after remediation.
    ///
    /// Intended for an operator/maintenance action once the permanent fault
    /// (embedder mismatch, backend auth/config) has been fixed. Passing `None`
    /// redrives every partition; a storage-partition id scopes the reset. Returns
    /// the number of rows re-queued.
    pub async fn redrive_dead_lettered_external_sync(
        &self,
        pool: &PgPool,
        storage_partition_id: Option<&str>,
    ) -> Result<u64> {
        redrive_dead_lettered_vector_sync(pool, storage_partition_id).await
    }

    async fn drain_external_sync_for_storage_partition(
        &self,
        pool: &PgPool,
        storage_partition_id: Option<&str>,
        limit: i64,
    ) -> Result<VectorSyncReport> {
        let jobs = claim_pending_vector_sync(pool, storage_partition_id, limit).await?;
        let mut report = VectorSyncReport {
            attempted: jobs.len() as u64,
            ..VectorSyncReport::default()
        };

        for (storage_partition_id, jobs) in group_jobs_by_partition(jobs) {
            let target = match self
                .external_for_storage_partition(pool, &storage_partition_id)
                .await
            {
                Ok(Some(target)) => target,
                Ok(None) => {
                    let job_refs = jobs.iter().collect::<Vec<_>>();
                    mark_vector_sync_processed_batch(pool, &job_refs).await?;
                    report.skipped += jobs.len() as u64;
                    continue;
                }
                Err(error) => {
                    let job_refs = jobs.iter().collect::<Vec<_>>();
                    fail_vector_sync_jobs(pool, &job_refs, &error, &mut report).await?;
                    continue;
                }
            };

            self.drain_external_sync_partition(
                pool,
                target,
                &storage_partition_id,
                &jobs,
                &mut report,
            )
            .await?;
        }

        Ok(report)
    }

    async fn drain_external_sync_partition(
        &self,
        pool: &PgPool,
        target: Arc<dyn VectorStore>,
        storage_partition_id: &str,
        jobs: &[crate::sync::VectorSyncJob],
        report: &mut VectorSyncReport,
    ) -> Result<()> {
        let upsert_jobs = jobs
            .iter()
            .filter(|job| job.operation == VectorSyncOperation::Upsert)
            .collect::<Vec<_>>();
        let delete_jobs = jobs
            .iter()
            .filter(|job| job.operation == VectorSyncOperation::Delete)
            .collect::<Vec<_>>();

        let mut delete_after_commit_jobs = Vec::new();
        let upsert_uids = upsert_jobs.iter().map(|job| job.uid).collect::<Vec<_>>();
        match fetch_current_vector_items(pool, storage_partition_id, &upsert_uids).await {
            Ok(items) => {
                let found_uids = items.iter().map(|item| item.uid).collect::<HashSet<_>>();
                let found_upsert_jobs = upsert_jobs
                    .iter()
                    .copied()
                    .filter(|job| found_uids.contains(&job.uid))
                    .collect::<Vec<_>>();
                delete_after_commit_jobs = upsert_jobs
                    .iter()
                    .copied()
                    .filter(|job| !found_uids.contains(&job.uid))
                    .collect::<Vec<_>>();
                if !items.is_empty() {
                    match target.upsert(&items).await {
                        Ok(()) => {
                            mark_vector_sync_processed_batch(pool, &found_upsert_jobs).await?;
                            report.succeeded += found_upsert_jobs.len() as u64;
                        }
                        Err(error) => {
                            fail_vector_sync_jobs(pool, &found_upsert_jobs, &error, report).await?;
                        }
                    }
                }
            }
            Err(error) => {
                fail_vector_sync_jobs(pool, &upsert_jobs, &error, report).await?;
            }
        }

        let mut delete_uids = Vec::new();
        let mut seen_delete_uids = HashSet::new();
        let mut delete_mark_jobs = Vec::new();
        for job in delete_jobs.into_iter().chain(delete_after_commit_jobs) {
            if seen_delete_uids.insert(job.uid) {
                delete_uids.push(job.uid);
            }
            delete_mark_jobs.push(job);
        }
        if delete_uids.is_empty() {
            return Ok(());
        }

        match target.delete(&delete_uids).await {
            Ok(()) => {
                mark_vector_sync_processed_batch(pool, &delete_mark_jobs).await?;
                report.succeeded += delete_mark_jobs.len() as u64;
            }
            Err(error) => {
                fail_vector_sync_jobs(pool, &delete_mark_jobs, &error, report).await?;
            }
        }
        Ok(())
    }

    /// Reports whether a storage partition routes vectors to an external backend.
    ///
    /// Transactional graph writes only queue `vector_sync_outbox` rows for
    /// partitions whose `vector_backend` is not `pgvector`, so pgvector-only
    /// partitions never have external sync work. Callers use this to skip the
    /// post-commit outbox drain entirely for those partitions. Missing
    /// `storage_partition_state` rows default to pgvector (returns `false`).
    pub async fn partition_uses_external_backend(
        &self,
        pool: &PgPool,
        storage_partition_id: &str,
    ) -> Result<bool> {
        let state = load_vector_backend_state(pool, storage_partition_id).await?;
        Ok(state.backend != "pgvector")
    }

    /// Returns the configured Turbopuffer client, when available.
    #[must_use]
    pub fn turbopuffer(&self) -> Option<Arc<TurbopufferStore>> {
        self.turbopuffer.clone()
    }

    async fn external_for_storage_partition(
        &self,
        pool: &PgPool,
        storage_partition_id: &str,
    ) -> Result<Option<Arc<dyn VectorStore>>> {
        let state = load_vector_backend_state(pool, storage_partition_id).await?;
        resolve_external_backend_choice(
            storage_partition_id,
            &state.backend,
            &state.hipaa_tier,
            self.turbopuffer.clone(),
        )
    }
}

/// Fails a batch of claimed vector-sync jobs, classifying transient vs permanent
/// errors and folding the outcome into the drain report.
///
/// Permanent failures (and transient ones that exhaust the attempt budget) are
/// quarantined and logged; recoverable failures back off for a later retry.
async fn fail_vector_sync_jobs(
    pool: &PgPool,
    jobs: &[&VectorSyncJob],
    error: &Error,
    report: &mut VectorSyncReport,
) -> Result<()> {
    let permanent = error.is_permanent();
    let dead_lettered = mark_vector_sync_failed_batch(pool, jobs, error, permanent).await?;
    report.failed += jobs.len() as u64;
    report.dead_lettered += dead_lettered;
    if dead_lettered > 0 {
        tracing::warn!(
            dead_lettered,
            permanent,
            error = %error,
            "quarantined vector-sync jobs after a permanent or exhausted failure"
        );
    }
    Ok(())
}

fn group_jobs_by_partition(jobs: Vec<VectorSyncJob>) -> BTreeMap<String, Vec<VectorSyncJob>> {
    let mut grouped = BTreeMap::new();
    for job in jobs {
        grouped
            .entry(job.storage_partition_id.clone())
            .or_insert_with(Vec::new)
            .push(job);
    }
    grouped
}

/// Vector backend bundle for transactional graph writes.
#[derive(Clone)]
pub struct TransactionalGraphVectorBackend {
    store: Arc<TransactionalGraphVectorStore>,
}

impl TransactionalGraphVectorBackend {
    /// Returns the pgvector-backed transactional vector store.
    #[must_use]
    pub fn vector_store(&self) -> Arc<dyn VectorStore> {
        self.store.clone()
    }

    /// Drains this graph-vector attachment's committed outbox rows to its backend.
    ///
    /// Used by batch ingestion paths (e.g. slow-path memory ingest) that want an
    /// explicit post-batch flush; ordinary graph writes are queue-only and rely
    /// on the background cron drainer instead.
    pub async fn sync_post_commit(&self) -> Result<()> {
        self.store.sync_post_commit().await
    }
}

struct TransactionalGraphVectorStore {
    source: Arc<PgvectorStore>,
    pool: PgPool,
    factory: VectorStoreFactory,
    storage_partition_id: String,
    post_commit_limit: i64,
}

#[async_trait]
impl VectorStore for TransactionalGraphVectorStore {
    fn backend(&self) -> &'static str {
        self.source.backend()
    }

    fn dimension(&self) -> usize {
        self.source.dimension()
    }

    async fn upsert(&self, items: &[VectorItem]) -> Result<()> {
        self.source.upsert(items).await
    }

    async fn upsert_in_tx(&self, conn: &mut PgConnection, items: &[VectorItem]) -> Result<()> {
        self.source.upsert_in_tx(conn, items).await?;
        let uids = items.iter().map(|item| item.uid).collect::<Vec<_>>();
        enqueue_external_vector_sync(
            conn,
            &self.storage_partition_id,
            VectorSyncOperation::Upsert,
            &uids,
        )
        .await
    }

    async fn knn(&self, query: &VectorQuery) -> Result<Vec<crate::VectorMatch>> {
        self.source.knn(query).await
    }

    async fn delete(&self, uids: &[Uuid]) -> Result<()> {
        self.source.delete(uids).await
    }

    async fn delete_in_tx(&self, conn: &mut PgConnection, uids: &[Uuid]) -> Result<()> {
        self.source.delete_in_tx(conn, uids).await?;
        enqueue_external_vector_sync(
            conn,
            &self.storage_partition_id,
            VectorSyncOperation::Delete,
            uids,
        )
        .await
    }
}

impl TransactionalGraphVectorStore {
    /// Drains this attachment's committed vector-sync outbox rows to its backend.
    async fn sync_post_commit(&self) -> Result<()> {
        self.factory
            .drain_external_sync_for_storage_partition(
                &self.pool,
                Some(&self.storage_partition_id),
                self.post_commit_limit,
            )
            .await?;
        Ok(())
    }
}

/// Selects the configured vector store for one storage partition.
///
/// Missing `storage_partition_state` rows default to pgvector. Storage partitions explicitly configured for
/// Turbopuffer require a configured client, and HIPAA-tier partitions additionally require that
/// the client was constructed with BAA enabled.
pub async fn vector_store_for_storage_partition(
    storage_partition_id: &str,
    pool: &PgPool,
    pgvector: Arc<PgvectorStore>,
    turbopuffer: Option<Arc<TurbopufferStore>>,
) -> Result<Arc<dyn VectorStore>> {
    let state = load_vector_backend_state(pool, storage_partition_id).await?;
    resolve_backend_choice(
        storage_partition_id,
        &state.backend,
        &state.hipaa_tier,
        pgvector,
        turbopuffer,
    )
}

struct VectorBackendState {
    backend: String,
    hipaa_tier: String,
}

async fn load_vector_backend_state(
    pool: &PgPool,
    storage_partition_id: &str,
) -> Result<VectorBackendState> {
    let row = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT vector_backend, hipaa_tier
        FROM moa.storage_partition_state
        WHERE storage_partition_id = $1
        "#,
    )
    .bind(storage_partition_id)
    .fetch_optional(pool)
    .await?;

    let (backend, hipaa_tier) =
        row.unwrap_or_else(|| ("pgvector".to_string(), "standard".to_string()));
    Ok(VectorBackendState {
        backend,
        hipaa_tier,
    })
}

fn resolve_backend_choice(
    storage_partition_id: &str,
    backend: &str,
    hipaa_tier: &str,
    pgvector: Arc<PgvectorStore>,
    turbopuffer: Option<Arc<TurbopufferStore>>,
) -> Result<Arc<dyn VectorStore>> {
    let Some(external) =
        resolve_external_backend_choice(storage_partition_id, backend, hipaa_tier, turbopuffer)?
    else {
        return Ok(pgvector);
    };
    Ok(external)
}

fn resolve_external_backend_choice(
    storage_partition_id: &str,
    backend: &str,
    hipaa_tier: &str,
    turbopuffer: Option<Arc<TurbopufferStore>>,
) -> Result<Option<Arc<dyn VectorStore>>> {
    match backend {
        "pgvector" => Ok(None),
        "turbopuffer" => {
            let store = turbopuffer.ok_or_else(|| Error::TurbopufferUnavailable {
                storage_partition_id: storage_partition_id.to_string(),
            })?;
            if matches!(hipaa_tier, "hipaa" | "restricted") && !store.has_baa() {
                return Err(Error::TurbopufferBaaRequired {
                    storage_partition_id: storage_partition_id.to_string(),
                });
            }
            let store: Arc<dyn VectorStore> =
                Arc::new(store.with_storage_partition_id(storage_partition_id.to_string()));
            Ok(Some(store))
        }
        other => Err(Error::UnsupportedVectorBackend {
            storage_partition_id: storage_partition_id.to_string(),
            backend: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moa_core::MoaConfig;
    use moa_core::RlsContext;
    use moa_core::TenantId;
    use secrecy::SecretString;
    use sqlx::PgPool;

    use crate::{
        Error, PgvectorStore, TurbopufferStore,
        backend::{VectorStoreFactory, resolve_backend_choice},
    };

    fn pg_store() -> Arc<PgvectorStore> {
        Arc::new(PgvectorStore::new(
            PgPool::connect_lazy("postgres://localhost/moa").expect("lazy pool"),
            RlsContext::tenant(TenantId::new()),
        ))
    }

    fn tp_store(baa_enabled: bool) -> Arc<TurbopufferStore> {
        Arc::new(
            TurbopufferStore::new(
                "http://127.0.0.1:1",
                SecretString::from("test-key".to_string()),
                "test",
                baa_enabled,
            )
            .expect("turbopuffer store"),
        )
    }

    #[tokio::test]
    async fn turbopuffer_selected_when_configured() {
        let selected = resolve_backend_choice(
            "w1",
            "turbopuffer",
            "standard",
            pg_store(),
            Some(tp_store(false)),
        )
        .expect("selection");
        assert_eq!(selected.backend(), "turbopuffer");
    }

    #[tokio::test]
    async fn hipaa_tier_requires_baa_enabled_turbopuffer_client() {
        let err = match resolve_backend_choice(
            "w1",
            "turbopuffer",
            "hipaa",
            pg_store(),
            Some(tp_store(false)),
        ) {
            Ok(store) => panic!("BAA gate should reject {}", store.backend()),
            Err(error) => error,
        };
        assert!(matches!(err, Error::TurbopufferBaaRequired { .. }));
    }

    #[tokio::test]
    async fn turbopuffer_backend_requires_configured_client() {
        // Pins: cloud storage partitions that select Turbopuffer fail clearly
        // instead of falling back to the local pgvector source.
        let err = match resolve_backend_choice("w1", "turbopuffer", "standard", pg_store(), None) {
            Ok(store) => panic!(
                "missing Turbopuffer client should reject {}",
                store.backend()
            ),
            Err(error) => error,
        };

        assert!(matches!(
            err,
            Error::TurbopufferUnavailable {
                storage_partition_id
            } if storage_partition_id == "w1"
        ));
    }

    #[tokio::test]
    async fn factory_transactional_graph_backend_uses_pgvector_source() {
        // Pins: graph writes keep a transaction-capable pgvector source even when
        // external vector backends are configured for read-side retrieval.
        let mut config = MoaConfig::default();
        config.memory.vector.turbopuffer.api_key = "test-key".to_string();
        let factory = VectorStoreFactory::from_config(&config);
        assert!(
            factory.turbopuffer().is_some(),
            "fixture should configure a Turbopuffer client"
        );

        let backend = factory.transactional_graph_backend(
            PgPool::connect_lazy("postgres://localhost/moa").expect("lazy pool"),
            RlsContext::tenant(TenantId::new()),
            true,
        );

        assert_eq!(backend.vector_store().backend(), "pgvector");
    }

    #[test]
    fn production_call_sites_do_not_construct_pgvector_directly() {
        // Pins: production ingestion, retrieval, and lifecycle paths route vector
        // construction through VectorStoreFactory instead of choosing pgvector ad hoc.
        let sources = [
            (
                "moa-brain pipeline memory",
                include_str!("../../../moa-brain/src/pipeline/memory.rs"),
            ),
            (
                "orchestrator knowledge ingest",
                include_str!("../../../moa-orchestrator/src/services/knowledge/ingest.rs"),
            ),
            (
                "orchestrator memory retrieval",
                include_str!("../../../moa-orchestrator/src/services/memory/retrieval.rs"),
            ),
            (
                "orchestrator admin maintenance",
                include_str!("../../../moa-orchestrator/src/services/admin_maintenance.rs"),
            ),
            (
                "memory fast path",
                include_str!("../../ingest/src/fast_path.rs"),
            ),
            (
                "memory slow path",
                include_str!("../../ingest/src/slow_path.rs"),
            ),
            (
                "memory lifecycle consolidation",
                include_str!("../../lifecycle/src/consolidate.rs"),
            ),
        ];
        let constructors = [
            "PgvectorStore::new(",
            "PgvectorStore::new_for_app_role(",
            "PgvectorStore::new_for_control_plane(",
        ];

        for (name, source) in sources {
            for constructor in constructors {
                assert!(
                    !source.contains(constructor),
                    "{name} must use VectorStoreFactory instead of {constructor}"
                );
            }
        }
    }
}
