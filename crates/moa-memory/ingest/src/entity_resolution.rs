//! Entity resolution helpers for slow-path graph-memory ingestion.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{ScopeContext, ScopedConn};
use moa_memory_graph::{GraphStore, NodeIndexRow, NodeLabel, NodeWriteIntent, PiiClass};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::Result;

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
}

/// Resolves extracted entity mentions to active graph `Entity` nodes.
#[derive(Clone)]
pub struct EntityResolver {
    verifier: Arc<dyn EntityMergeVerifier>,
    assume_app_role: bool,
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

    /// Resolves one entity mention, creating an `Entity` node when no active match exists.
    pub async fn resolve(
        &self,
        pool: &PgPool,
        graph: &dyn GraphStore,
        request: EntityResolutionRequest<'_>,
    ) -> Result<ResolvedEntity> {
        let normalized_name = normalize_entity_name(request.name);
        let candidates = self
            .lookup_block_candidates(pool, request.scope, &normalized_name)
            .await?;
        if let Some(candidate) = self.match_candidate(request.name, &candidates).await? {
            return Ok(ResolvedEntity {
                uid: candidate.uid,
                name: candidate.name,
                normalized_name,
                created: false,
            });
        }

        let display_name = display_entity_name(request.name, &normalized_name);
        let uid = Uuid::now_v7();
        graph
            .create_node(NodeWriteIntent {
                uid,
                label: NodeLabel::Entity,
                workspace_id: request
                    .scope
                    .workspace_id()
                    .map(|workspace_id| workspace_id.to_string()),
                user_id: request.scope.user_id().map(|user_id| user_id.to_string()),
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
                embedding: None,
                embedding_model: None,
                embedding_model_version: None,
                actor_id: request.actor_id.to_string(),
                actor_kind: request.actor_kind.to_string(),
            })
            .await?;

        Ok(ResolvedEntity {
            uid,
            name: display_name,
            normalized_name,
            created: true,
        })
    }

    async fn lookup_block_candidates(
        &self,
        pool: &PgPool,
        scope: &ScopeContext,
        normalized_name: &str,
    ) -> Result<Vec<NodeIndexRow>> {
        let workspace_id = scope
            .workspace_id()
            .map(|workspace_id| workspace_id.to_string());
        let user_id = scope.user_id().map(|user_id| user_id.to_string());
        let mut conn = ScopedConn::begin(pool, scope).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        let rows = sqlx::query_as::<_, NodeIndexRow>(
            r#"
            SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
                   valid_to, valid_from, properties_summary, last_accessed_at
            FROM moa.node_index
            WHERE valid_to IS NULL
              AND label = $1
              AND scope = $2
              AND (($3::text IS NULL AND workspace_id IS NULL) OR workspace_id = $3)
              AND (($4::text IS NULL AND user_id IS NULL) OR user_id = $4)
            ORDER BY valid_from ASC, uid ASC
            "#,
        )
        .bind(NodeLabel::Entity.as_str())
        .bind(scope.tier_str())
        .bind(workspace_id.as_deref())
        .bind(user_id.as_deref())
        .fetch_all(conn.as_mut())
        .await?;
        conn.commit().await?;

        Ok(rows
            .into_iter()
            .filter(|row| normalize_entity_name(&row.name) == normalized_name)
            .collect())
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
}

/// Inputs needed to resolve one extracted entity mention.
#[derive(Debug, Clone)]
pub struct EntityResolutionRequest<'a> {
    /// Request scope used for RLS and entity ownership.
    pub scope: &'a ScopeContext,
    /// Extracted subject or object mention.
    pub name: &'a str,
    /// PII class inherited from the redacted fact.
    pub pii_class: PiiClass,
    /// Confidence assigned to the entity node when one is created.
    pub confidence: f64,
    /// Application-time validity start for a newly created entity.
    pub valid_from: DateTime<Utc>,
    /// Actor identifier written to graph changelog rows.
    pub actor_id: &'a str,
    /// Actor kind written to graph changelog rows.
    pub actor_kind: &'a str,
}

fn display_entity_name(name: &str, normalized_name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        normalized_name.to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_entity_name(name: &str) -> String {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in name.chars() {
        if character.is_alphanumeric() {
            token.extend(character.to_lowercase());
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }

    let normalized = tokens.join(" ");
    if normalized.is_empty() {
        name.trim().to_lowercase()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_entity_name;

    #[test]
    fn entity_name_normalization_blocks_case_and_punctuation_variants() {
        // Pins: entity blocking treats punctuation and case changes as the same mention.
        assert_eq!(normalize_entity_name(" API_Service "), "api service");
        assert_eq!(normalize_entity_name("api-service"), "api service");
        assert_eq!(normalize_entity_name("api.service"), "api service");
    }
}
