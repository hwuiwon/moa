//! Entity resolution helpers for slow-path graph-memory ingestion.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{traits::EmbeddingProvider, types::identifiers::StoragePartitionId};
use moa_db::ScopedConn;
use moa_memory_graph::{NodeIndexRow, NodeLabel, NodeWriteIntent};
use moa_memory_types::normalize_entity_name;
use moa_memory_vector::{VectorQuery, VectorStore};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{Error, Result};

const EMBEDDING_BLOCK_K: usize = 5;

/// Verifies whether an extracted entity mention should merge into an existing candidate.
#[async_trait]
pub trait EntityMergeVerifier: Send + Sync {
    /// Returns whether `mention` and `candidate` refer to the same entity.
    async fn should_merge(&self, mention: &str, candidate: &NodeIndexRow) -> Result<bool>;
}

/// Deterministic verifier that merges candidates with the same normalized name.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeterministicEntityMergeVerifier;

#[async_trait]
impl EntityMergeVerifier for DeterministicEntityMergeVerifier {
    async fn should_merge(&self, mention: &str, candidate: &NodeIndexRow) -> Result<bool> {
        Ok(normalize_entity_name(mention) == normalize_entity_name(&candidate.name))
    }
}

/// Outcome of planning one entity resolution: the resolved endpoint plus, when a
/// new entity node must be written, the intent to create it.
///
/// The plan performs only reads. Callers apply `create` inside their own
/// transaction (for example the per-fact transaction in slow-path ingestion) so
/// entity node creation commits atomically with the fact node and edges rather
/// than in a separate transaction. This keeps the resolver free of the
/// `GraphStore` write abstraction.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityResolutionPlan {
    /// Resolved entity endpoint used for edge writes.
    pub resolved: ResolvedEntity,
    /// Node intent to create when the mention resolved to a new entity.
    pub create: Option<NodeWriteIntent>,
}

/// Resolved graph entity endpoint for an extracted fact subject or object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEntity {
    /// Stable graph node uid.
    pub uid: Uuid,
    /// Canonical display name stored on the entity node.
    pub name: String,
    /// Blocking key used for deterministic entity matching.
    pub normalized_name: String,
    /// Whether this resolution created a new graph node.
    pub created: bool,
    /// Raw mention that should be recorded as an alias on the newly written merge edge.
    pub alias_mention: Option<String>,
}

/// Resolves extracted entity mentions to active graph `Entity` nodes.
#[derive(Clone)]
pub struct EntityResolver {
    verifier: Arc<dyn EntityMergeVerifier>,
    assume_app_role: bool,
    embedding_blocker: Option<EmbeddingBlocker>,
}

#[derive(Clone)]
struct EmbeddingBlocker {
    embedder: Arc<dyn EmbeddingProvider>,
    vector: Arc<dyn VectorStore>,
    cosine_threshold: f32,
}

#[derive(Debug, Clone)]
struct EmbeddingCandidate {
    row: NodeIndexRow,
}

impl Default for EntityResolver {
    fn default() -> Self {
        Self::new(Arc::new(DeterministicEntityMergeVerifier))
    }
}

impl EntityResolver {
    /// Creates an entity resolver with the provided merge verifier.
    #[must_use]
    pub fn new(verifier: Arc<dyn EntityMergeVerifier>) -> Self {
        Self {
            verifier,
            assume_app_role: false,
            embedding_blocker: None,
        }
    }

    /// Creates a deterministic entity resolver for test and local ingestion paths.
    #[must_use]
    pub fn deterministic() -> Self {
        Self::default()
    }

    /// Creates a deterministic resolver that assumes `moa_app` inside scoped transactions.
    #[must_use]
    pub fn deterministic_for_app_role() -> Self {
        Self::deterministic().with_assume_app_role(true)
    }

    /// Creates an entity resolver that assumes `moa_app` inside scoped transactions.
    #[must_use]
    pub fn for_app_role(verifier: Arc<dyn EntityMergeVerifier>) -> Self {
        Self::new(verifier).with_assume_app_role(true)
    }

    /// Returns a copy of this resolver with the app-role assumption changed.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    /// Returns a copy of this resolver that uses embedding KNN to block candidates after exact misses.
    #[must_use]
    pub fn with_embedding_blocking(
        mut self,
        embedder: Arc<dyn EmbeddingProvider>,
        vector: Arc<dyn VectorStore>,
        cosine_threshold: f64,
    ) -> Self {
        self.embedding_blocker = Some(EmbeddingBlocker {
            embedder,
            vector,
            cosine_threshold: cosine_threshold as f32,
        });
        self
    }

    /// Plans one entity mention's resolution, returning a node-create intent when
    /// no active match exists.
    ///
    /// This performs only reads (block-candidate and embedding-candidate lookups
    /// each in their own short transaction). The returned
    /// [`EntityResolutionPlan::create`] intent, when present, is written by the
    /// caller inside its own transaction so entity creation composes atomically
    /// with the fact node and edges. When the resolver has embedding blocking
    /// enabled, [`EntityResolutionRequest::precomputed_embedding`] is reused for
    /// both the KNN lookup and the created node's vector; otherwise the name is
    /// embedded once here.
    pub async fn plan_resolution(
        &self,
        pool: &PgPool,
        request: EntityResolutionRequest<'_>,
    ) -> Result<EntityResolutionPlan> {
        let normalized_name = normalize_entity_name(request.name);
        let candidates = self
            .lookup_block_candidates(pool, request.scope, &normalized_name)
            .await?;
        if let Some(candidate) = self.match_candidate(request.name, &candidates).await? {
            return Ok(EntityResolutionPlan {
                resolved: ResolvedEntity {
                    uid: candidate.uid,
                    name: candidate.name,
                    normalized_name,
                    created: false,
                    alias_mention: None,
                },
                create: None,
            });
        }

        let mut mention_embedding = None;
        if let Some(blocker) = &self.embedding_blocker {
            let embedding = match request.precomputed_embedding {
                Some(embedding) => embedding.to_vec(),
                None => {
                    self.embed_normalized_name(blocker, &normalized_name)
                        .await?
                }
            };
            mention_embedding = Some(embedding.clone());
            let candidates = self
                .lookup_embedding_candidates(pool, request.scope, request.pii_class, embedding)
                .await?;
            if let Some(candidate) = self
                .match_embedding_candidate(request.name, &candidates)
                .await?
            {
                let alias_mention = alias_mention(request.name, &candidate.name);
                return Ok(EntityResolutionPlan {
                    resolved: ResolvedEntity {
                        uid: candidate.uid,
                        normalized_name: normalize_entity_name(&candidate.name),
                        name: candidate.name,
                        created: false,
                        alias_mention,
                    },
                    create: None,
                });
            }
        }

        let display_name = display_entity_name(request.name, &normalized_name);
        let uid = deterministic_entity_uid(request.scope, &normalized_name);
        let (embedding, embedding_model, embedding_model_version) =
            if let Some(blocker) = &self.embedding_blocker {
                let embedding = match mention_embedding {
                    Some(embedding) => embedding,
                    None => {
                        self.embed_normalized_name(blocker, &normalized_name)
                            .await?
                    }
                };
                (
                    Some(embedding),
                    Some(blocker.embedder.model_id().to_string()),
                    Some(blocker.embedder.model_version()),
                )
            } else {
                (None, None, None)
            };
        let create = NodeWriteIntent {
            barrier: request.barrier.cloned(),
            uid,
            data_subject_id: request
                .scope
                .contact_id()
                .map_or(request.scope.tenant_id().0, |contact_id| contact_id.0),
            label: NodeLabel::Entity,
            storage_partition_id: Some(
                StoragePartitionId::for_tenant(request.scope.tenant_id()).to_string(),
            ),
            contact_id: request
                .scope
                .contact_id()
                .map(|contact_id| contact_id.to_string()),
            scope: request.scope.tier_str().to_string(),
            name: display_name.clone(),
            properties: json!({
                "uid": uid.to_string(),
                "name": display_name.clone(),
                "normalized_name": normalized_name.clone(),
                "source": "slow_path_entity_resolution",
            }),
            pii_class: request.pii_class,
            confidence: Some(request.confidence),
            valid_from: request.valid_from,
            embedding,
            embedding_model,
            embedding_model_version,
            embedding_text: None,
            actor_id: request.actor_id.to_string(),
            actor_kind: request.actor_kind.to_string(),
        };

        Ok(EntityResolutionPlan {
            resolved: ResolvedEntity {
                uid,
                name: display_name,
                normalized_name,
                created: true,
                alias_mention: None,
            },
            create: Some(create),
        })
    }

    async fn lookup_block_candidates(
        &self,
        pool: &PgPool,
        scope: &RlsContext,
        normalized_name: &str,
    ) -> Result<Vec<NodeIndexRow>> {
        // Entity nodes written by this resolver are content-addressed by
        // `(scope, normalized_name)` through `deterministic_entity_uid`, so the
        // canonical block candidate is a primary-key lookup on that uid. This
        // replaces the previous full scan of every active `Entity` row in scope
        // followed by a Rust-side normalized-name filter (run twice per fact, for
        // the subject and object). The scope/label guards and the defensive
        // normalized-name check are retained so a uid collision cannot return an
        // unrelated node.
        let candidate_uid = deterministic_entity_uid(scope, normalized_name);
        let storage_partition_id = Some(scope.tenant_id().to_string());
        let user_id = scope.contact_id().map(|contact_id| contact_id.to_string());
        let mut conn = ScopedConn::begin_as_app(pool, scope, self.assume_app_role).await?;
        let rows = sqlx::query_as::<_, NodeIndexRow>(
            r#"
            SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class,
                   valid_to, valid_from, properties_summary, last_accessed_at,
                   COALESCE(quality_score, 0.5) AS quality_score
            FROM moa.node_index
            WHERE uid = $1
              AND valid_to IS NULL
              AND label = $2
              AND scope = $3
              AND (($4::text IS NULL AND storage_partition_id IS NULL) OR storage_partition_id = $4)
              AND (($5::text IS NULL AND user_id IS NULL) OR user_id = $5)
            LIMIT 1
            "#,
        )
        .bind(candidate_uid)
        .bind(NodeLabel::Entity.as_str())
        .bind(scope.tier_str())
        .bind(storage_partition_id.as_deref())
        .bind(user_id.as_deref())
        .fetch_all(conn.as_mut())
        .await?;
        conn.commit().await?;

        Ok(rows
            .into_iter()
            .filter(|row| normalize_entity_name(&row.name) == normalized_name)
            .collect())
    }

    async fn lookup_embedding_candidates(
        &self,
        pool: &PgPool,
        scope: &RlsContext,
        pii_class: SensitivityClass,
        embedding: Vec<f32>,
    ) -> Result<Vec<EmbeddingCandidate>> {
        let Some(blocker) = &self.embedding_blocker else {
            return Ok(Vec::new());
        };
        let mut matches = blocker
            .vector
            .knn(&VectorQuery {
                embedding,
                k: EMBEDDING_BLOCK_K,
                label_filter: Some(vec![NodeLabel::Entity.as_str().to_string()]),
                max_pii_class: pii_class,
                include_global: false,
                as_of: None,
            })
            .await?
            .into_iter()
            .filter(|candidate| candidate.score >= blocker.cosine_threshold)
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.uid.cmp(&right.uid))
        });
        if matches.is_empty() {
            return Ok(Vec::new());
        }

        let storage_partition_id = Some(scope.tenant_id().to_string());
        let user_id = scope.contact_id().map(|contact_id| contact_id.to_string());
        let uids = matches
            .iter()
            .map(|candidate| candidate.uid)
            .collect::<Vec<_>>();
        let mut conn = ScopedConn::begin_as_app(pool, scope, self.assume_app_role).await?;
        let rows = sqlx::query_as::<_, NodeIndexRow>(
            r#"
            SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class,
                   valid_to, valid_from, properties_summary, last_accessed_at,
                   COALESCE(quality_score, 0.5) AS quality_score
            FROM moa.node_index
            WHERE valid_to IS NULL
              AND label = $1
              AND uid = ANY($2)
              AND scope = $3
              AND (($4::text IS NULL AND storage_partition_id IS NULL) OR storage_partition_id = $4)
              AND (($5::text IS NULL AND user_id IS NULL) OR user_id = $5)
            "#,
        )
        .bind(NodeLabel::Entity.as_str())
        .bind(&uids)
        .bind(scope.tier_str())
        .bind(storage_partition_id.as_deref())
        .bind(user_id.as_deref())
        .fetch_all(conn.as_mut())
        .await?;
        conn.commit().await?;

        let mut rows_by_uid = rows
            .into_iter()
            .map(|row| (row.uid, row))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut ordered = Vec::new();
        for candidate in matches {
            if let Some(row) = rows_by_uid.remove(&candidate.uid) {
                ordered.push(EmbeddingCandidate { row });
            }
        }
        Ok(ordered)
    }

    async fn match_candidate(
        &self,
        mention: &str,
        candidates: &[NodeIndexRow],
    ) -> Result<Option<NodeIndexRow>> {
        if let [candidate] = candidates {
            return Ok(Some(candidate.clone()));
        }

        for candidate in candidates {
            if self.verifier.should_merge(mention, candidate).await? {
                return Ok(Some(candidate.clone()));
            }
        }
        Ok(None)
    }

    async fn match_embedding_candidate(
        &self,
        mention: &str,
        candidates: &[EmbeddingCandidate],
    ) -> Result<Option<NodeIndexRow>> {
        for candidate in candidates {
            if self.verifier.should_merge(mention, &candidate.row).await? {
                return Ok(Some(candidate.row.clone()));
            }
        }
        Ok(None)
    }

    async fn embed_normalized_name(
        &self,
        blocker: &EmbeddingBlocker,
        normalized_name: &str,
    ) -> Result<Vec<f32>> {
        let embeddings = blocker
            .embedder
            .embed(&[normalized_name.to_string()])
            .await
            .map_err(|error| {
                Error::EntityResolution(format!(
                    "failed to embed entity name `{normalized_name}`: {error}"
                ))
            })?;
        embeddings.into_iter().next().ok_or_else(|| {
            Error::EntityResolution(format!(
                "embedder returned no vector for entity name `{normalized_name}`"
            ))
        })
    }
}

/// Inputs needed to resolve one extracted entity mention.
#[derive(Debug, Clone)]
pub struct EntityResolutionRequest<'a> {
    /// Request scope used for RLS and entity ownership.
    pub scope: &'a RlsContext,
    /// Extracted subject or object mention.
    pub name: &'a str,
    /// PII class inherited from the redacted fact.
    pub pii_class: SensitivityClass,
    /// Confidence assigned to the entity node when one is created.
    pub confidence: f64,
    /// Application-time validity start for a newly created entity.
    pub valid_from: DateTime<Utc>,
    /// Actor identifier written to graph changelog rows.
    pub actor_id: &'a str,
    /// Actor kind written to graph changelog rows.
    pub actor_kind: &'a str,
    /// Information-barrier tag inherited from the ingestion session, persisted on
    /// any newly created entity node so it is need-to-know restricted alongside
    /// the fact that mentioned it. `None` leaves the entity unrestricted.
    pub barrier: Option<&'a moa_core::types::memory::InformationBarrierId>,
    /// Embedding of the mention's normalized name, precomputed for the whole fact
    /// batch in one provider call.
    ///
    /// When `Some`, the resolver reuses it instead of embedding the name again;
    /// when `None` it falls back to a single-name embed. Ignored when embedding
    /// blocking is disabled.
    pub precomputed_embedding: Option<&'a [f32]>,
}

fn display_entity_name(name: &str, normalized_name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        normalized_name.to_string()
    } else {
        trimmed.to_string()
    }
}

fn alias_mention(mention: &str, candidate_name: &str) -> Option<String> {
    let trimmed = mention.trim();
    (!trimmed.is_empty() && trimmed != candidate_name.trim()).then(|| trimmed.to_string())
}

fn deterministic_entity_uid(scope: &RlsContext, normalized_name: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(b"moa:entity:v1");
    hasher.update([0]);
    hasher.update(scope.tier_str().as_bytes());
    hasher.update([0]);
    hasher.update(scope.tenant_id().to_string().as_bytes());
    hasher.update([0]);
    if let Some(contact_id) = scope.contact_id() {
        hasher.update(contact_id.to_string().as_bytes());
    }
    hasher.update([0]);
    hasher.update(normalized_name.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use moa_core::types::memory::RlsContext;
    use moa_core::{types::contact::ContactId, types::identifiers::TenantId};
    use moa_memory_types::normalize_entity_name;
    use uuid::Uuid;

    use super::deterministic_entity_uid;

    #[test]
    fn deterministic_entity_uid_is_stable_inside_scope() {
        // Pins: eval graph expansion is not perturbed by fresh entity UUIDs.
        let scope = RlsContext::tenant(TenantId::from(Uuid::from_u128(0x1000)));
        let normalized = normalize_entity_name("Lib Audit Wire");

        let first = deterministic_entity_uid(&scope, &normalized);
        let second = deterministic_entity_uid(&scope, &normalized);

        assert_eq!(first, second);
    }

    #[test]
    fn deterministic_entity_uid_includes_contact_scope() {
        // Pins: same entity text in different contact scopes does not alias.
        let tenant_id = TenantId::from(Uuid::from_u128(0x1000));
        let contact_a = RlsContext::contact(tenant_id, ContactId(Uuid::from_u128(0x2000)));
        let contact_b = RlsContext::contact(tenant_id, ContactId(Uuid::from_u128(0x2001)));
        let normalized = normalize_entity_name("repo/search-platform");

        let uid_a = deterministic_entity_uid(&contact_a, &normalized);
        let uid_b = deterministic_entity_uid(&contact_b, &normalized);

        assert_ne!(uid_a, uid_b);
    }
}
