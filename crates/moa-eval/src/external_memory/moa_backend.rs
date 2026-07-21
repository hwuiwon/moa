//! MOA production-path adapter for the backend-neutral benchmark contract.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use moa_brain::pipeline::MemoryEvidenceRequest;
use moa_brain::pipeline::memory::GraphMemoryRetriever;
use moa_core::traits::{EmbeddingProvider, Identity, IdentityType};
use moa_core::{
    types::contact::ContactId,
    types::contact::ContactRef,
    types::contact::ContactVerificationState,
    types::context::{TURN_ID_METADATA_KEY, WorkingContext},
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::identifiers::TenantId,
    types::memory::RlsContext,
    types::model::ModelCapabilities,
    types::session::SessionMeta,
};
use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_db::ScopedConn;
use moa_memory_graph::{GraphStore, PostgresGraphStore};
use moa_memory_ingest::{
    ContradictionDetector, EntityResolver, FactExtractor, HeuristicFactExtractor, IngestCtx,
    RrfPlusJudgeDetector, SessionTurn, ingest_turn_direct_with_ctx,
};
use moa_memory_lifecycle::{ConsolidationOptions, consolidate_tenant};
use moa_memory_pii::{HeuristicPiiClassifier, PiiClassifier};
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION, VectorStore};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::dataset::ChronologicalTurn;
use super::harness::{
    EvidenceExport, EvidenceOccurrenceRef, EvidenceSourceRef, ExternalMemoryBackend,
};
use super::{ExternalMemoryError, Result};

/// MOA backend using production slow-path ingestion, lifecycle, admission, and rendering.
pub struct MoaMemoryBackend {
    pool: PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    embedder: Arc<dyn EmbeddingProvider>,
    extractor: Arc<dyn FactExtractor>,
    pii: Arc<dyn PiiClassifier>,
    contradiction_detector: Arc<dyn ContradictionDetector>,
    entity_resolver: Arc<EntityResolver>,
    entity_blocking_enabled: bool,
    consolidation: ConsolidationOptions,
    retriever: GraphMemoryRetriever,
    run_namespace: String,
    reset_generation: u64,
    active: Option<ActiveIsolation>,
}

struct ActiveIsolation {
    key: String,
    run_namespace: String,
    tenant_id: TenantId,
    contact_id: ContactId,
    query_session_id: SessionId,
    ingest_ctx: IngestCtx,
    internal_sessions: HashMap<String, SessionId>,
    external_sessions: HashMap<SessionId, String>,
    external_turns: HashMap<(SessionId, u64), String>,
    latest_timestamp: Option<DateTime<Utc>>,
    retrieval_sequence: u64,
}

impl MoaMemoryBackend {
    /// Creates a backend with explicit embedding plus deterministic local formation dependencies.
    pub fn new(
        pool: PgPool,
        embedder: Arc<dyn EmbeddingProvider>,
        consolidation: ConsolidationOptions,
    ) -> Result<Self> {
        let kms: Arc<dyn KeyManagementProvider> = Arc::new(LocalKmsProvider::new());
        Self::new_with_dependencies(
            pool,
            kms,
            embedder,
            Arc::new(HeuristicFactExtractor),
            Arc::new(HeuristicPiiClassifier),
            Arc::new(RrfPlusJudgeDetector::default()),
            Arc::new(EntityResolver::deterministic_for_app_role()),
            false,
            consolidation,
        )
    }

    /// Creates a backend from fully explicit production ingestion dependencies.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_dependencies(
        pool: PgPool,
        kms: Arc<dyn KeyManagementProvider>,
        embedder: Arc<dyn EmbeddingProvider>,
        extractor: Arc<dyn FactExtractor>,
        pii: Arc<dyn PiiClassifier>,
        contradiction_detector: Arc<dyn ContradictionDetector>,
        entity_resolver: Arc<EntityResolver>,
        entity_blocking_enabled: bool,
        consolidation: ConsolidationOptions,
    ) -> Result<Self> {
        if embedder.dimensions() != VECTOR_DIMENSION || embedder.model_id().trim().is_empty() {
            return Err(ExternalMemoryError::InvalidConfig(format!(
                "MOA backend requires an explicit configured {VECTOR_DIMENSION}-dimension embedder"
            )));
        }
        let retriever = GraphMemoryRetriever::new_with_config(
            moa_core::config::MoaConfig::default(),
            pool.clone(),
            kms.clone(),
            Some(embedder.clone()),
        )
        .with_assume_app_role(true);
        Ok(Self {
            pool,
            kms,
            embedder,
            extractor,
            pii,
            contradiction_detector,
            entity_resolver,
            entity_blocking_enabled,
            consolidation,
            retriever,
            run_namespace: Uuid::now_v7().to_string(),
            reset_generation: 0,
            active: None,
        })
    }

    fn build_active_isolation(&self, key: &str, generation: u64) -> ActiveIsolation {
        let tenant_id = TenantId(deterministic_uuid(
            b"moa.external-memory.tenant.v1",
            &[&self.run_namespace, key, &generation.to_string()],
        ));
        let contact_id = ContactId(deterministic_uuid(
            b"moa.external-memory.contact.v1",
            &[&self.run_namespace, key, &generation.to_string()],
        ));
        let query_session_id = SessionId(deterministic_uuid(
            b"moa.external-memory.query-session.v1",
            &[&self.run_namespace, key, &generation.to_string()],
        ));
        let scope = RlsContext::contact(tenant_id, contact_id);
        let vector: Arc<dyn VectorStore> = Arc::new(PgvectorStore::new_for_app_role(
            self.pool.clone(),
            scope.clone(),
        ));
        let graph: Arc<dyn GraphStore> = Arc::new(
            PostgresGraphStore::scoped_for_app_role(self.pool.clone(), scope, self.kms.clone())
                .with_vector_store(vector.clone()),
        );
        let ingest_ctx = IngestCtx::new(
            self.pool.clone(),
            self.kms.clone(),
            graph,
            vector,
            self.embedder.clone(),
            self.pii.clone(),
            self.contradiction_detector.clone(),
        )
        .with_extractor(self.extractor.clone())
        .with_entity_resolver(self.entity_resolver.clone())
        .with_entity_embedding_blocking(self.entity_blocking_enabled);
        ActiveIsolation {
            key: key.to_string(),
            run_namespace: self.run_namespace.clone(),
            tenant_id,
            contact_id,
            query_session_id,
            ingest_ctx,
            internal_sessions: HashMap::new(),
            external_sessions: HashMap::new(),
            external_turns: HashMap::new(),
            latest_timestamp: None,
            retrieval_sequence: 0,
        }
    }

    fn working_context(active: &ActiveIsolation) -> WorkingContext {
        let session = SessionMeta {
            id: active.query_session_id,
            tenant_id: active.tenant_id,
            model: ModelId::new("external-memory-reader"),
            contact: Some(ContactRef {
                contact_id: active.contact_id,
                tenant_id: active.tenant_id,
                state: ContactVerificationState::Verified,
                canonical_contact_id: None,
                linked_contact_ids: Vec::new(),
                scopes: Vec::new(),
                permissions: Value::Null,
                agent_ids: Vec::new(),
                session_ids: Vec::new(),
                verified_contact_point_ids: Vec::new(),
            }),
            ..SessionMeta::default()
        };
        let mut context = WorkingContext::new(
            &session,
            ModelCapabilities {
                model_id: ModelId::new("external-memory-reader"),
                context_window: 32_768,
                max_output: 1_024,
                ..ModelCapabilities::default()
            },
        );
        context.set_caller_identity(Identity {
            identity_type: IdentityType::Contact,
            id: active.contact_id.0,
            tenant_id: active.tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        });
        let turn_id = deterministic_uuid(
            b"moa.external-memory.query-turn.v1",
            &[
                &active.query_session_id.to_string(),
                &active.retrieval_sequence.to_string(),
            ],
        );
        context.insert_metadata(TURN_ID_METADATA_KEY, Value::String(turn_id.to_string()));
        context
    }
}

#[async_trait]
impl ExternalMemoryBackend for MoaMemoryBackend {
    async fn reset(&mut self, isolation_key: &str) -> std::result::Result<(), String> {
        if isolation_key.trim().is_empty() {
            return Err("isolation key must not be blank".to_string());
        }
        self.reset_generation = self.reset_generation.saturating_add(1);
        let active = self.build_active_isolation(isolation_key, self.reset_generation);
        seed_storage_partition_embedder_state(&self.pool, active.tenant_id, self.embedder.as_ref())
            .await?;
        self.active = Some(active);
        Ok(())
    }

    async fn ingest(&mut self, turn: &ChronologicalTurn) -> std::result::Result<(), String> {
        let Some(active) = self.active.as_mut() else {
            return Err("reset must run before ingest".to_string());
        };
        let session_id = *active
            .internal_sessions
            .entry(turn.session_source_id.clone())
            .or_insert_with(|| {
                SessionId(deterministic_uuid(
                    b"moa.external-memory.source-session.v1",
                    &[&active.run_namespace, &active.key, &turn.session_source_id],
                ))
            });
        active
            .external_sessions
            .entry(session_id)
            .or_insert_with(|| turn.session_source_id.clone());
        let turn_seq = u64::try_from(turn.turn_source_order)
            .map_err(|error| format!("turn source order does not fit u64: {error}"))?
            .saturating_add(1);
        if active
            .external_turns
            .insert((session_id, turn_seq), turn.turn_source_id.clone())
            .is_some()
        {
            return Err(format!(
                "duplicate internal source turn {}:{turn_seq}",
                turn.session_source_id
            ));
        }
        let report = ingest_turn_direct_with_ctx(
            active.ingest_ctx.clone(),
            SessionTurn {
                tenant_id: active.tenant_id,
                contact_id: Some(active.contact_id),
                session_id,
                turn_seq,
                transcript: turn.text.clone(),
                dominant_pii_class: "none".to_string(),
                finalized_at: turn.occurred_at,
                barrier: None,
            },
        )
        .await
        .map_err(|error| format!("production slow-path ingest failed: {error:?}"))?;
        if report.failed > 0 {
            return Err(format!(
                "production slow-path ingest retained {} failed facts",
                report.failed
            ));
        }
        active.latest_timestamp = Some(
            active
                .latest_timestamp
                .map_or(turn.occurred_at, |latest| latest.max(turn.occurred_at)),
        );
        Ok(())
    }

    async fn settle(&mut self) -> std::result::Result<(), String> {
        let Some(active) = self.active.as_ref() else {
            return Err("reset must run before settle".to_string());
        };
        let now = active
            .latest_timestamp
            .unwrap_or_else(Utc::now)
            .checked_add_signed(Duration::seconds(1))
            .ok_or_else(|| "settle timestamp overflowed".to_string())?;
        consolidate_tenant(
            &self.pool,
            self.kms.clone(),
            active.tenant_id,
            self.consolidation.clone(),
            now,
            Some(self.embedder.clone()),
        )
        .await
        .map_err(|error| format!("production consolidation failed: {error}"))?;
        Ok(())
    }

    async fn retrieve(
        &mut self,
        query: &str,
        evidence_token_budget: usize,
        ranked_occurrence_depth: usize,
    ) -> std::result::Result<EvidenceExport, String> {
        let Some(active) = self.active.as_mut() else {
            return Err("reset must run before retrieve".to_string());
        };
        active.retrieval_sequence = active.retrieval_sequence.saturating_add(1);
        let context = Self::working_context(active);
        let response = self
            .retriever
            .retrieve_evidence(
                &context,
                MemoryEvidenceRequest::new(query, evidence_token_budget)
                    .with_ranked_occurrence_depth(ranked_occurrence_depth)
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| format!("production evidence retrieval failed: {error}"))?;
        if response.source_metadata.len() != response.hits.len()
            || response
                .source_metadata
                .iter()
                .zip(&response.hits)
                .any(|(metadata, hit)| metadata.graph_uid != hit.uid)
        {
            return Err("production evidence metadata is not rank-aligned".to_string());
        }
        let mut ranked_graph_occurrences = Vec::with_capacity(response.source_metadata.len());
        for metadata in &response.source_metadata {
            let source_session_id = metadata.source_session_id.ok_or_else(|| {
                format!(
                    "graph hit {} has no typed source session",
                    metadata.graph_uid
                )
            })?;
            let source_turn_seq = metadata.source_turn_seq.ok_or_else(|| {
                format!("graph hit {} has no typed source turn", metadata.graph_uid)
            })?;
            let external_session = active
                .external_sessions
                .get(&source_session_id)
                .ok_or_else(|| {
                    format!(
                        "graph hit {} belongs to an unmapped internal session",
                        metadata.graph_uid
                    )
                })?;
            let external_turn = active
                .external_turns
                .get(&(source_session_id, source_turn_seq))
                .ok_or_else(|| {
                    format!(
                        "graph hit {} belongs to an unmapped internal turn",
                        metadata.graph_uid
                    )
                })?;
            ranked_graph_occurrences.push((
                metadata.graph_uid,
                EvidenceOccurrenceRef {
                    session_source_id: external_session.clone(),
                    turn_source_id: external_turn.clone(),
                },
            ));
        }
        let rendered_graph_refs = response
            .source_refs
            .iter()
            .map(|source_ref| {
                let graph_uid = source_ref.source_uid.ok_or_else(|| {
                    "rendered graph-memory source ref omitted its graph uid".to_string()
                })?;
                let evidence = source_ref.excerpt.clone().ok_or_else(|| {
                    format!("graph hit {graph_uid} has no rendered evidence excerpt")
                })?;
                Ok((graph_uid, evidence))
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        let (ranked_source_refs, rendered_source_refs) =
            project_evidence_sources(ranked_graph_occurrences, rendered_graph_refs)?;
        Ok(EvidenceExport {
            rendered_evidence: response.rendered_evidence,
            tokens_used: response.consumed_evidence_tokens,
            ranked_source_refs,
            rendered_source_refs,
        })
    }
}

fn project_evidence_sources(
    ranked_graph_occurrences: Vec<(Uuid, EvidenceOccurrenceRef)>,
    rendered_graph_refs: Vec<(Uuid, String)>,
) -> std::result::Result<(Vec<EvidenceOccurrenceRef>, Vec<EvidenceSourceRef>), String> {
    let mut occurrence_by_graph = HashMap::new();
    let mut seen_occurrences = HashSet::new();
    let mut ranked_source_refs = Vec::new();
    for (graph_uid, occurrence) in ranked_graph_occurrences {
        if occurrence_by_graph
            .insert(graph_uid, occurrence.clone())
            .is_some()
        {
            return Err(format!("duplicate ranked graph hit {graph_uid}"));
        }
        if seen_occurrences.insert(occurrence.clone()) {
            ranked_source_refs.push(occurrence);
        }
    }

    let mut rendered_source_refs = Vec::with_capacity(rendered_graph_refs.len());
    for (graph_uid, evidence) in rendered_graph_refs {
        let occurrence = occurrence_by_graph.get(&graph_uid).ok_or_else(|| {
            format!("rendered graph hit {graph_uid} was absent from admitted ranked hits")
        })?;
        rendered_source_refs.push(EvidenceSourceRef {
            session_source_id: occurrence.session_source_id.clone(),
            turn_source_id: occurrence.turn_source_id.clone(),
            evidence,
        });
    }
    Ok((ranked_source_refs, rendered_source_refs))
}

async fn seed_storage_partition_embedder_state(
    pool: &PgPool,
    tenant_id: TenantId,
    embedder: &dyn EmbeddingProvider,
) -> std::result::Result<(), String> {
    let scope = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .map_err(|error| format!("begin embedder-state transaction: {error}"))?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .map_err(|error| format!("assume app role for embedder state: {error}"))?;
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
    .bind(tenant_id.to_string())
    .bind(embedder.model_id())
    .bind(embedder.model_version())
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .map_err(|error| format!("seed explicit embedder state: {error}"))?;
    conn.commit()
        .await
        .map_err(|error| format!("commit explicit embedder state: {error}"))?;
    Ok(())
}

fn deterministic_uuid(domain: &[u8], parts: &[&str]) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod ranked_occurrence {
    use super::{EvidenceOccurrenceRef, project_evidence_sources};
    use uuid::Uuid;

    fn occurrence(session: &str, turn: &str) -> EvidenceOccurrenceRef {
        EvidenceOccurrenceRef {
            session_source_id: session.to_string(),
            turn_source_id: turn.to_string(),
        }
    }

    #[test]
    fn ranked_occurrence_stable_collapses_graph_facts_and_keeps_rendered_repeats() {
        // Pins: retrieval metrics see one first-ranked source occurrence while
        // the reader retains every rendered graph-fact excerpt.
        let first = Uuid::from_u128(1);
        let repeated = Uuid::from_u128(2);
        let later = Uuid::from_u128(3);
        let (ranked, rendered) = project_evidence_sources(
            vec![
                (first, occurrence("s1", "t1")),
                (repeated, occurrence("s1", "t1")),
                (later, occurrence("s2", "t2")),
            ],
            vec![
                (first, "fact one".to_string()),
                (repeated, "fact two".to_string()),
            ],
        )
        .expect("valid projection");

        assert_eq!(ranked, vec![occurrence("s1", "t1"), occurrence("s2", "t2")]);
        assert_eq!(rendered.len(), 2);
        assert!(rendered.iter().all(|source| source.turn_source_id == "t1"));
        assert_eq!(rendered[0].evidence, "fact one");
        assert_eq!(rendered[1].evidence, "fact two");
    }
}
