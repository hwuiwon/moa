//! Isolated eval-store lifecycle and deterministic stored-state preparation.

use super::*;

pub(crate) struct LoadedMemoryEvalCorpus {
    pub(crate) manifest: CorpusManifest,
    pub(crate) ledger: Vec<LedgerFact>,
    pub(crate) sessions: Vec<SyntheticSession>,
    pub(crate) probes: Vec<Probe>,
    pub(crate) embedding_inputs: Vec<EmbeddingInput>,
    pub(crate) embeddings: Vec<CachedEmbeddingFixture>,
}

impl LoadedMemoryEvalCorpus {
    pub(crate) async fn load(corpus_dir: &Path) -> Result<Self> {
        Self::load_for_lane(corpus_dir, EvalLane::Pr).await
    }

    pub(crate) async fn load_for_lane(corpus_dir: &Path, lane: EvalLane) -> Result<Self> {
        let manifest = read_manifest_json(&corpus_dir.join("manifest.json")).await?;
        let ledger = read_ledger_jsonl(&corpus_dir.join("ledger.jsonl")).await?;
        let sessions = read_sessions_jsonl(&corpus_dir.join("sessions.jsonl")).await?;
        let probes = read_probes_jsonl(&corpus_dir.join("probes.jsonl"), &ledger).await?;
        validate_corpus(&manifest, &ledger, &sessions, &probes)?;
        let (embedding_inputs, embeddings) = match lane {
            EvalLane::Pr => {
                let embedding_inputs = read_embedding_inputs_jsonl(
                    &corpus_dir.join("embedding_inputs.jsonl"),
                    &ledger,
                    &probes,
                )
                .await?;
                let embeddings =
                    read_embeddings_jsonl(&corpus_dir.join("embeddings.jsonl")).await?;
                (embedding_inputs, embeddings)
            }
            EvalLane::Live => (Vec::new(), Vec::new()),
        };
        Ok(Self {
            manifest,
            ledger,
            sessions,
            probes,
            embedding_inputs,
            embeddings,
        })
    }
}

pub(crate) struct IsolatedEvalStore {
    pub(super) store: PostgresSessionStore,
    pub(super) kms: Arc<dyn KeyManagementProvider>,
    pub(super) database_url: String,
    pub(super) schema_name: String,
}

impl IsolatedEvalStore {
    pub(crate) async fn create() -> Result<Self> {
        let maintenance_url = test_database_url()?;
        let (database_url, schema_name) =
            moa_session::testing::provision_cloned_database_from(&maintenance_url).await?;
        let store =
            match PostgresSessionStore::new_in_existing_schema(&database_url, &schema_name).await {
                Ok(store) => store,
                Err(error) => {
                    if let Err(cleanup_error) =
                        moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
                    {
                        tracing::warn!(
                            %cleanup_error,
                            "failed to clean up memory eval clone after store initialization failed"
                        );
                    }
                    return Err(error.into());
                }
            };
        Ok(Self {
            store,
            kms: Arc::new(LocalKmsProvider::new()),
            database_url,
            schema_name,
        })
    }

    pub(crate) fn pool(&self) -> &PgPool {
        self.store.pool()
    }

    pub(crate) fn ingest_ctx(
        &self,
        embedder: Arc<dyn EmbeddingProvider>,
        extractor: Arc<dyn FactExtractor>,
        entity_merge_verifier: Arc<dyn EntityMergeVerifier>,
        entity_blocking_enabled: bool,
    ) -> IngestCtx {
        let tenant_id = tenant_id_from_label(&format!("memory-eval-runner-{}", self.schema_name));
        let scope = RlsContext::tenant(tenant_id);
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            self.pool().clone(),
            scope.clone(),
        ));
        let graph = Arc::new(
            PostgresGraphStore::scoped_for_app_role(self.pool().clone(), scope, self.kms.clone())
                .with_vector_store(vector.clone()),
        );
        let entity_resolver = EntityResolver::for_app_role(entity_merge_verifier);
        IngestCtx::new(
            self.pool().clone(),
            self.kms.clone(),
            graph,
            vector,
            embedder,
            Arc::new(MemoryEvalPiiClassifier),
            Arc::new(InsertOnlyContradictionDetector),
        )
        .with_extractor(extractor)
        .with_entity_resolver(Arc::new(entity_resolver))
        .with_entity_embedding_blocking(entity_blocking_enabled)
    }

    pub(crate) async fn cleanup(self) -> Result<()> {
        let pool = self.store.pool().clone();
        drop(self.store);
        pool.close().await;
        moa_session::testing::cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .map_err(EvalError::from)
    }
}

pub(super) fn test_database_url() -> Result<String> {
    env::var("MOA_DATABASE_URL").map_err(|_| {
        EvalError::InvalidConfig(
            "MOA_DATABASE_URL must be set for memory retrieval eval".to_string(),
        )
    })
}

#[derive(Debug, Clone)]
struct MemoryEvalPiiClassifier;

#[async_trait]
impl PiiClassifier for MemoryEvalPiiClassifier {
    async fn classify(&self, text: &str) -> std::result::Result<PiiResult, PiiError> {
        Ok(deterministic_pii_result(text))
    }
}

pub(super) fn deterministic_pii_result(text: &str) -> PiiResult {
    let mut spans = Vec::new();
    let mut cursor = 0_usize;
    for token in text.split_whitespace() {
        let Some(offset) = text[cursor..].find(token) else {
            continue;
        };
        let start = cursor + offset;
        let end = start + token.len();
        cursor = end;
        if token.contains('@') {
            spans.push(PiiSpan::new(start, end, PiiCategory::Email, 0.95));
        } else if token.contains("sk-") || token.to_ascii_lowercase().contains("secret") {
            spans.push(PiiSpan::new(start, end, PiiCategory::Secret, 0.90));
        }
    }

    PiiResult {
        class: if spans.is_empty() {
            SensitivityClass::None
        } else {
            SensitivityClass::Pii
        },
        spans,
        model_version: "memory-eval-deterministic-pii-v1".to_string(),
        abstained: false,
    }
}

#[derive(Debug, Clone)]
struct InsertOnlyContradictionDetector;

#[async_trait]
impl ContradictionDetector for InsertOnlyContradictionDetector {
    async fn check_one_fast(
        &self,
        _fact_text: &str,
        _embedding: &[f32],
        _label: moa_memory_graph::NodeLabel,
        _pii_class: SensitivityClass,
        _ctx: &ContradictionContext,
    ) -> std::result::Result<Conflict, Error> {
        Ok(Conflict::Insert)
    }

    async fn check_one_slow(
        &self,
        _fact: &EmbeddedFact,
        _ctx: &ContradictionContext,
    ) -> std::result::Result<Conflict, Error> {
        Ok(Conflict::Insert)
    }
}

pub(crate) async fn cleanup_eval_graph_rows(pool: &PgPool, ledger: &[LedgerFact]) -> Result<()> {
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    if storage_partition_ids.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM moa.edge_index WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query("DELETE FROM moa.embeddings WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query("DELETE FROM moa.ingest_dlq WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query("DELETE FROM moa.ingest_dedup WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query("DELETE FROM moa.memory_digests WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query("DELETE FROM moa.retrieval_lineage WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query("DELETE FROM moa.graph_changelog WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query("DELETE FROM moa.node_index WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query("DELETE FROM moa.storage_partition_state WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
    Ok(())
}

pub(super) async fn seed_eval_storage_partition_embedder_state(
    pool: &PgPool,
    ledger: &[LedgerFact],
    embedder: &dyn EmbeddingProvider,
) -> Result<()> {
    for storage_partition_id in eval_storage_partition_ids(ledger) {
        seed_eval_storage_partition_embedder_state_row(pool, &storage_partition_id, embedder)
            .await?;
    }
    Ok(())
}

pub(super) async fn seed_eval_storage_partition_embedder_state_row(
    pool: &PgPool,
    storage_partition_id: &str,
    embedder: &dyn EmbeddingProvider,
) -> Result<()> {
    let scope = RlsContext::tenant(tenant_id_from_storage_partition(storage_partition_id));
    let mut conn = ScopedConn::begin(pool, &scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .map_err(crate::eval_sqlx_error)?;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady',
                updated_at = now()
        "#,
    )
    .bind(storage_partition_id)
    .bind(embedder.model_id())
    .bind(embedder.model_version())
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .map_err(crate::eval_sqlx_error)?;
    conn.commit().await?;
    Ok(())
}

pub(super) fn eval_storage_partition_ids(ledger: &[LedgerFact]) -> Vec<String> {
    ledger
        .iter()
        .map(|fact| tenant_id_from_storage_partition_id(&fact.storage_partition_id).to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(super) async fn apply_eval_validity_windows(
    pool: &PgPool,
    gold_resolution: &mut GoldResolutionReport,
) -> Result<()> {
    for record in &mut gold_resolution.records {
        let Some(valid_to) = record.expected_valid_to else {
            continue;
        };
        if record.node_uids.is_empty() {
            continue;
        }

        sqlx::query("UPDATE moa.node_index SET valid_to = $1 WHERE uid = ANY($2)")
            .bind(valid_to)
            .bind(&record.node_uids)
            .execute(pool)
            .await
            .map_err(crate::eval_sqlx_error)?;
        sqlx::query("UPDATE moa.embeddings SET valid_to = $1 WHERE uid = ANY($2)")
            .bind(valid_to)
            .bind(&record.node_uids)
            .execute(pool)
            .await
            .map_err(crate::eval_sqlx_error)?;

        record.valid_to = Some(valid_to);
        record.active = false;
        for node in &mut record.nodes {
            node.valid_to = Some(valid_to);
            node.active = false;
        }
    }
    Ok(())
}

pub(super) async fn stabilize_eval_access_times(
    pool: &PgPool,
    ledger: &[LedgerFact],
) -> Result<()> {
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    if storage_partition_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE moa.node_index SET last_accessed_at = valid_from WHERE storage_partition_id = ANY($1)",
    )
    .bind(&storage_partition_ids)
    .execute(pool)
    .await
    .map_err(crate::eval_sqlx_error)?;
    Ok(())
}

pub(super) async fn seed_eval_quality_scores(
    pool: &PgPool,
    ledger: &[LedgerFact],
    gold_resolution: &GoldResolutionReport,
    invert_priors: bool,
) -> Result<()> {
    let facts = ledger_by_fact_id(ledger);
    for record in &gold_resolution.records {
        let Some(fact) = facts.get(record.fact_id.as_str()) else {
            continue;
        };
        let (Some(uses), Some(successes)) = (fact.prior_uses, fact.prior_successes) else {
            continue;
        };
        if record.node_uids.is_empty() {
            continue;
        }
        let successes = if invert_priors {
            uses.saturating_sub(successes)
        } else {
            successes
        };
        let quality_score = beta_smoothed_quality(u64::from(uses), u64::from(successes));
        sqlx::query("UPDATE moa.node_index SET quality_score = $1 WHERE uid = ANY($2)")
            .bind(quality_score)
            .bind(&record.node_uids)
            .execute(pool)
            .await
            .map_err(crate::eval_sqlx_error)?;
    }
    Ok(())
}
