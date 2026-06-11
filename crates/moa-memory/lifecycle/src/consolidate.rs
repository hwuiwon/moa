//! Consolidation pass logic shared by workflows and eval runners.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use moa_core::{ScopeContext, UserId, WorkspaceId, traits::EmbeddingProvider};
use moa_memory_graph::{
    AgeGraphStore, ExistingSupersessionIntent, GraphError, NodeEmbeddingIntent, NodeLabel,
    NodePropertyUpdateIntent, PiiClass,
};
use moa_memory_ingest::normalize_entity_name;
use moa_memory_vector::PgvectorStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const CONSOLIDATION_ACTOR: &str = "consolidation";
const CONSOLIDATION_ACTOR_KIND: &str = "system";
const EPSILON: f64 = 1e-9;

/// Result type returned by lifecycle helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Error returned by graph-memory lifecycle operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A graph write failed.
    #[error("graph: {0}")]
    Graph(#[from] GraphError),
    /// A SQL query failed.
    #[error("sql: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Embedding provider call failed.
    #[error("embedding: {0}")]
    Embedding(#[from] moa_core::MoaError),
    /// Stored graph row was malformed.
    #[error("invalid graph row: {0}")]
    InvalidRow(String),
}

/// Tuning options for confidence decay.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationOptions {
    /// Number of idle days before confidence begins to decay.
    pub decay_idle_days: i64,
    /// Half-life applied to idle facts after the anchor confidence is captured.
    pub decay_half_life_days: f64,
    /// Minimum confidence retained by decay.
    pub decay_floor: f64,
}

impl Default for ConsolidationOptions {
    fn default() -> Self {
        Self {
            decay_idle_days: 30,
            decay_half_life_days: 180.0,
            decay_floor: 0.1,
        }
    }
}

/// Serializable outcome for one workspace consolidation pass.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationOutcome {
    /// Duplicate fact nodes invalidated into a canonical active node.
    pub merged: u64,
    /// Facts whose confidence was lowered during this pass.
    pub decayed: u64,
    /// Active facts whose computed confidence is at the configured floor.
    pub at_floor: u64,
    /// Older contradictory facts superseded by the newest fact.
    pub contradiction_supersessions: u64,
    /// Entity nodes that received missing embeddings.
    pub entity_embeddings_backfilled: u64,
    /// Alias mentions promoted from edge properties to entity node aliases.
    pub aliases_promoted: u64,
    /// Exact-duplicate groups that still have more than one active node after merge.
    pub duplicates_remaining: u64,
}

impl ConsolidationOutcome {
    /// Returns whether the pass performed no mutating work.
    #[must_use]
    pub fn has_no_work(&self) -> bool {
        self.merged == 0
            && self.decayed == 0
            && self.contradiction_supersessions == 0
            && self.entity_embeddings_backfilled == 0
            && self.aliases_promoted == 0
            && self.duplicates_remaining == 0
    }
}

/// Outcome for exact-duplicate merge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeStats {
    /// Duplicate nodes invalidated into canonical nodes.
    pub merged: u64,
    /// Duplicate groups still active after the pass.
    pub duplicates_remaining: u64,
}

/// Outcome for anchored confidence decay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecayStats {
    /// Facts whose confidence was lowered.
    pub decayed: u64,
    /// Facts currently sitting at the configured floor.
    pub at_floor: u64,
}

/// Outcome for deterministic contradiction sweep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepStats {
    /// Supersession operations written for contradictory facts.
    pub contradiction_supersessions: u64,
}

/// Outcome for entity vector and alias backfill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillStats {
    /// Entity nodes that received missing embeddings.
    pub entity_embeddings_backfilled: u64,
    /// Alias mentions promoted onto entity node properties.
    pub aliases_promoted: u64,
}

/// Runs all v1 consolidation operations for one workspace.
pub async fn consolidate_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    opts: ConsolidationOptions,
    now: DateTime<Utc>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) -> Result<ConsolidationOutcome> {
    let merge = merge_duplicates(pool, &workspace_id, now).await?;
    let decay = decay_confidence(pool, &workspace_id, now, &opts).await?;
    let sweep = sweep_contradictions(pool, &workspace_id, now).await?;
    let backfill = backfill_entities(pool, &workspace_id, embedder).await?;

    Ok(ConsolidationOutcome {
        merged: merge.merged,
        decayed: decay.decayed,
        at_floor: decay.at_floor,
        contradiction_supersessions: sweep.contradiction_supersessions,
        entity_embeddings_backfilled: backfill.entity_embeddings_backfilled,
        aliases_promoted: backfill.aliases_promoted,
        duplicates_remaining: merge.duplicates_remaining,
    })
}

/// Merges active exact-duplicate facts by `(workspace, user, scope, fact_hash)`.
pub async fn merge_duplicates(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    now: DateTime<Utc>,
) -> Result<MergeStats> {
    let facts = active_fact_rows(pool, workspace_id).await?;
    let groups = duplicate_groups(&facts);
    let mut merged = 0_u64;

    for group in groups {
        let canonical = &group[0];
        for duplicate in group.iter().skip(1) {
            close_into_existing(
                pool,
                duplicate,
                canonical,
                duplicate.valid_from,
                "merged",
                now,
            )
            .await?;
            merged += 1;
        }
    }

    let remaining = duplicate_groups(&active_fact_rows(pool, workspace_id).await?)
        .into_iter()
        .filter(|group| group.len() > 1)
        .count() as u64;
    Ok(MergeStats {
        merged,
        duplicates_remaining: remaining,
    })
}

/// Applies anchored confidence decay to idle active facts.
pub async fn decay_confidence(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    now: DateTime<Utc>,
    opts: &ConsolidationOptions,
) -> Result<DecayStats> {
    let facts = active_fact_rows(pool, workspace_id).await?;
    let mut decayed = 0_u64;
    let mut at_floor = 0_u64;

    for fact in facts {
        let Some(current) = fact.confidence else {
            continue;
        };
        let Some(target) = decay_target(&fact, current, now, opts) else {
            continue;
        };
        if (target - opts.decay_floor).abs() <= EPSILON {
            at_floor += 1;
        }
        if (target - current).abs() <= EPSILON {
            continue;
        }

        let mut properties = fact.properties_object();
        properties
            .entry("base_confidence".to_string())
            .or_insert_with(|| json!(current));
        properties.insert("confidence".to_string(), json!(target));
        scoped_graph(pool, &fact)
            .update_node_properties(NodePropertyUpdateIntent {
                uid: fact.uid,
                properties: Value::Object(properties),
                confidence: Some(target),
                actor_id: CONSOLIDATION_ACTOR.to_string(),
                actor_kind: CONSOLIDATION_ACTOR_KIND.to_string(),
            })
            .await?;
        decayed += 1;
    }

    Ok(DecayStats { decayed, at_floor })
}

/// Supersedes older active contradictory facts using a deterministic newest-wins policy.
pub async fn sweep_contradictions(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    now: DateTime<Utc>,
) -> Result<SweepStats> {
    let facts = active_fact_rows(pool, workspace_id).await?;
    let mut supersessions = 0_u64;

    for group in contradiction_groups(&facts) {
        let keeper = &group[0];
        for old in group.iter().skip(1) {
            let valid_to = if keeper.valid_from > old.valid_from {
                keeper.valid_from
            } else {
                old.valid_from
            };
            close_into_existing(pool, old, keeper, valid_to, "contradiction_sweep", now).await?;
            supersessions += 1;
        }
    }

    Ok(SweepStats {
        contradiction_supersessions: supersessions,
    })
}

/// Backfills missing entity embeddings and promotes edge alias mentions to entity properties.
pub async fn backfill_entities(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) -> Result<BackfillStats> {
    let entities = active_entity_rows(pool, workspace_id).await?;
    let mut embeddings = 0_u64;
    let mut aliases = 0_u64;

    if let Some(embedder) = embedder {
        let missing = entities
            .iter()
            .filter(|entity| !entity.has_embedding)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            let inputs = missing
                .iter()
                .map(|entity| normalized_entity_name(entity))
                .collect::<Vec<_>>();
            let vectors = embedder.embed(&inputs).await?;
            for (entity, vector) in missing.into_iter().zip(vectors) {
                scoped_graph(pool, entity)
                    .upsert_node_embedding(NodeEmbeddingIntent {
                        uid: entity.uid,
                        embedding: vector,
                        embedding_model: embedder.model_name().to_string(),
                        embedding_model_version: embedder.model_version(),
                        actor_id: CONSOLIDATION_ACTOR.to_string(),
                        actor_kind: CONSOLIDATION_ACTOR_KIND.to_string(),
                    })
                    .await?;
                embeddings += 1;
            }
        }
    } else {
        tracing::debug!(
            workspace_id = %workspace_id,
            "entity embedding backfill skipped because no embedder was provided"
        );
    }

    for entity in &entities {
        let promoted = promote_aliases_for_entity(pool, entity).await?;
        aliases += promoted;
    }

    Ok(BackfillStats {
        entity_embeddings_backfilled: embeddings,
        aliases_promoted: aliases,
    })
}

async fn close_into_existing(
    pool: &PgPool,
    old: &LifecycleNodeRow,
    replacement: &LifecycleNodeRow,
    valid_to: DateTime<Utc>,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    scoped_graph(pool, old)
        .close_existing_node_with_supersession(ExistingSupersessionIntent {
            old_uid: old.uid,
            replacement_uid: replacement.uid,
            valid_to,
            invalidated_at: now,
            reason: reason.to_string(),
            actor_id: CONSOLIDATION_ACTOR.to_string(),
            actor_kind: CONSOLIDATION_ACTOR_KIND.to_string(),
        })
        .await?;
    Ok(())
}

fn duplicate_groups(rows: &[LifecycleNodeRow]) -> Vec<Vec<LifecycleNodeRow>> {
    let mut groups = BTreeMap::<DuplicateKey, Vec<LifecycleNodeRow>>::new();
    for row in rows {
        let Some(fact_hash) = row.property_text("fact_hash") else {
            continue;
        };
        groups
            .entry(DuplicateKey {
                workspace_id: row.workspace_id.clone(),
                user_id: row.user_id.clone(),
                scope: row.scope.clone(),
                fact_hash,
            })
            .or_default()
            .push(row.clone());
    }

    let mut values = groups
        .into_values()
        .filter(|group| group.len() > 1)
        .map(|mut group| {
            group.sort_by_key(|row| (row.valid_from, row.uid));
            group
        })
        .collect::<Vec<_>>();
    values.sort_by_key(|left| duplicate_group_key(left));
    values
}

fn contradiction_groups(rows: &[LifecycleNodeRow]) -> Vec<Vec<LifecycleNodeRow>> {
    let mut groups = BTreeMap::<ContradictionKey, Vec<LifecycleNodeRow>>::new();
    for row in rows {
        let (Some(subject), Some(predicate), Some(object)) = (
            row.property_text("subject"),
            row.property_text("predicate"),
            row.property_text("object"),
        ) else {
            continue;
        };
        groups
            .entry(ContradictionKey {
                workspace_id: row.workspace_id.clone(),
                user_id: row.user_id.clone(),
                scope: row.scope.clone(),
                subject,
                predicate,
            })
            .or_default()
            .push(row.clone().with_cached_object(object));
    }

    let mut values = groups
        .into_values()
        .filter(|group| {
            group
                .iter()
                .filter_map(|row| row.cached_object.as_deref())
                .collect::<BTreeSet<_>>()
                .len()
                > 1
        })
        .map(|mut group| {
            group.sort_by_key(|row| (std::cmp::Reverse(row.valid_from), row.uid));
            group
        })
        .collect::<Vec<_>>();
    values.sort_by_key(|left| contradiction_group_key(left));
    values
}

fn decay_target(
    fact: &LifecycleNodeRow,
    current: f64,
    now: DateTime<Utc>,
    opts: &ConsolidationOptions,
) -> Option<f64> {
    let idle = now.signed_duration_since(fact.last_accessed_at);
    if idle < Duration::days(opts.decay_idle_days) {
        return None;
    }
    if opts.decay_half_life_days <= 0.0 || !opts.decay_half_life_days.is_finite() {
        return None;
    }
    let base = fact
        .properties
        .as_ref()
        .and_then(|value| value.get("base_confidence"))
        .and_then(Value::as_f64)
        .unwrap_or(current);
    let idle_days = idle.num_seconds().max(0) as f64 / 86_400.0;
    Some(
        (base * 0.5_f64.powf(idle_days / opts.decay_half_life_days))
            .max(opts.decay_floor)
            .clamp(0.0, 1.0),
    )
}

async fn promote_aliases_for_entity(pool: &PgPool, entity: &LifecycleNodeRow) -> Result<u64> {
    let aliases = edge_aliases_for_entity(pool, entity).await?;
    if aliases.is_empty() {
        return Ok(0);
    }
    let mut properties = entity.properties_object();
    let mut existing = properties
        .get("aliases")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let before = existing.len();
    existing.extend(aliases);
    let added = existing.len().saturating_sub(before);
    if added == 0 {
        return Ok(0);
    }
    properties.insert(
        "aliases".to_string(),
        Value::Array(existing.into_iter().map(Value::String).collect()),
    );
    scoped_graph(pool, entity)
        .update_node_properties(NodePropertyUpdateIntent {
            uid: entity.uid,
            properties: Value::Object(properties),
            confidence: None,
            actor_id: CONSOLIDATION_ACTOR.to_string(),
            actor_kind: CONSOLIDATION_ACTOR_KIND.to_string(),
        })
        .await?;
    Ok(added as u64)
}

async fn edge_aliases_for_entity(
    pool: &PgPool,
    entity: &LifecycleNodeRow,
) -> Result<BTreeSet<String>> {
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT payload->'after'->>'alias_mention' AS alias
        FROM moa.graph_changelog
        WHERE workspace_id = $1
          AND target_kind = 'edge'
          AND op = 'create'
          AND (payload->>'start_uid' = $2 OR payload->>'end_uid' = $2)
          AND payload->'after' ? 'alias_mention'
        "#,
    )
    .bind(entity.workspace_id.as_deref())
    .bind(entity.uid.to_string())
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| row.try_get::<Option<String>, _>("alias").ok().flatten())
        .map(|alias| alias.trim().to_string())
        .filter(|alias| !alias.is_empty())
        .collect())
}

async fn active_fact_rows(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
) -> Result<Vec<LifecycleNodeRow>> {
    active_rows(pool, workspace_id, NodeLabel::Fact, false).await
}

async fn active_entity_rows(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
) -> Result<Vec<LifecycleNodeRow>> {
    active_rows(pool, workspace_id, NodeLabel::Entity, true).await
}

async fn active_rows(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    label: NodeLabel,
    include_embedding_state: bool,
) -> Result<Vec<LifecycleNodeRow>> {
    let rows = sqlx::query(
        r#"
        SELECT node.uid,
               node.workspace_id,
               node.user_id,
               node.scope,
               node.name,
               node.pii_class,
               node.confidence,
               node.valid_from,
               node.valid_to,
               node.properties_summary,
               node.last_accessed_at,
               (embedding.uid IS NOT NULL) AS has_embedding
        FROM moa.node_index AS node
        LEFT JOIN moa.embeddings AS embedding
          ON embedding.uid = node.uid
         AND embedding.valid_to IS NULL
        WHERE node.workspace_id = $1
          AND node.label = $2
          AND node.valid_to IS NULL
        ORDER BY node.scope ASC, node.user_id ASC NULLS FIRST, node.valid_from ASC, node.uid ASC
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(label.as_str())
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| lifecycle_row_from_sql(row, include_embedding_state))
        .collect()
}

fn lifecycle_row_from_sql(
    row: sqlx::postgres::PgRow,
    include_embedding_state: bool,
) -> Result<LifecycleNodeRow> {
    let pii_class: String = row.try_get("pii_class")?;
    Ok(LifecycleNodeRow {
        uid: row.try_get("uid")?,
        workspace_id: row.try_get("workspace_id")?,
        user_id: row.try_get("user_id")?,
        scope: row.try_get("scope")?,
        name: row.try_get("name")?,
        pii_class: pii_class
            .parse::<PiiClass>()
            .map_err(|error| Error::InvalidRow(error.to_string()))?,
        confidence: row.try_get("confidence")?,
        valid_from: row.try_get("valid_from")?,
        valid_to: row.try_get("valid_to")?,
        properties: row.try_get("properties_summary")?,
        last_accessed_at: row.try_get("last_accessed_at")?,
        has_embedding: include_embedding_state && row.try_get("has_embedding")?,
        cached_object: None,
    })
}

fn scoped_graph(pool: &PgPool, row: &LifecycleNodeRow) -> AgeGraphStore {
    let scope = row.scope_context();
    let vector = Arc::new(PgvectorStore::new(pool.clone(), scope.clone()));
    AgeGraphStore::scoped(pool.clone(), scope).with_vector_store(vector)
}

fn normalized_entity_name(entity: &LifecycleNodeRow) -> String {
    entity
        .properties
        .as_ref()
        .and_then(|properties| properties.get("normalized_name"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| normalize_entity_name(&entity.name))
}

fn duplicate_group_key(group: &[LifecycleNodeRow]) -> DuplicateKey {
    let row = &group[0];
    DuplicateKey {
        workspace_id: row.workspace_id.clone(),
        user_id: row.user_id.clone(),
        scope: row.scope.clone(),
        fact_hash: row.property_text("fact_hash").unwrap_or_default(),
    }
}

fn contradiction_group_key(group: &[LifecycleNodeRow]) -> ContradictionKey {
    let row = &group[0];
    ContradictionKey {
        workspace_id: row.workspace_id.clone(),
        user_id: row.user_id.clone(),
        scope: row.scope.clone(),
        subject: row.property_text("subject").unwrap_or_default(),
        predicate: row.property_text("predicate").unwrap_or_default(),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct LifecycleNodeRow {
    uid: Uuid,
    workspace_id: Option<String>,
    user_id: Option<String>,
    scope: String,
    name: String,
    pii_class: PiiClass,
    confidence: Option<f64>,
    valid_from: DateTime<Utc>,
    valid_to: Option<DateTime<Utc>>,
    properties: Option<Value>,
    last_accessed_at: DateTime<Utc>,
    has_embedding: bool,
    cached_object: Option<String>,
}

impl LifecycleNodeRow {
    fn property_text(&self, key: &str) -> Option<String> {
        self.properties
            .as_ref()
            .and_then(|properties| properties.get(key))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }

    fn properties_object(&self) -> serde_json::Map<String, Value> {
        self.properties
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    fn scope_context(&self) -> ScopeContext {
        let workspace_id = WorkspaceId::new(self.workspace_id.clone().unwrap_or_default());
        match self.user_id.as_ref() {
            Some(user_id) => ScopeContext::user(workspace_id, UserId::new(user_id.clone())),
            None => ScopeContext::workspace(workspace_id),
        }
    }

    fn with_cached_object(mut self, object: String) -> Self {
        self.cached_object = Some(object);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DuplicateKey {
    workspace_id: Option<String>,
    user_id: Option<String>,
    scope: String,
    fact_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ContradictionKey {
    workspace_id: Option<String>,
    user_id: Option<String>,
    scope: String,
    subject: String,
    predicate: String,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;

    #[test]
    fn merge_groups_by_scope_ownership_and_fact_hash() {
        // Pins: exact duplicate grouping never crosses user or workspace scope.
        let rows = vec![
            scoped_fact("a", None, "workspace", "hash-1", 0),
            scoped_fact("b", None, "workspace", "hash-1", 1),
            scoped_fact("c", Some("user-a"), "user", "hash-1", 2),
            scoped_fact("d", Some("user-a"), "user", "hash-1", 3),
            scoped_fact("e", Some("user-b"), "user", "hash-1", 4),
        ];

        let groups = duplicate_groups(&rows);

        assert_eq!(groups.len(), 2);
        assert_eq!(uids(&groups[0]), vec![uuid("a"), uuid("b")]);
        assert_eq!(uids(&groups[1]), vec![uuid("c"), uuid("d")]);
    }

    #[test]
    fn merge_canonical_is_earliest_valid_from_with_uid_tiebreak() {
        // Pins: exact duplicate canonical selection is stable across database row order.
        let earliest = scoped_fact(
            "00000000-0000-8000-8000-000000000002",
            None,
            "workspace",
            "h",
            0,
        );
        let uid_tiebreak = scoped_fact(
            "00000000-0000-8000-8000-000000000001",
            None,
            "workspace",
            "h",
            0,
        );
        let later = scoped_fact(
            "00000000-0000-8000-8000-000000000003",
            None,
            "workspace",
            "h",
            2,
        );

        let groups = duplicate_groups(&[later, earliest, uid_tiebreak]);

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0][0].uid,
            uuid("00000000-0000-8000-8000-000000000001")
        );
    }

    #[test]
    fn decay_anchors_to_base_confidence_and_is_idempotent_at_same_instant() {
        // Pins: rerunning decay at the same instant computes the same anchored target.
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 0, 0, 0).unwrap();
        let opts = ConsolidationOptions::default();
        let mut row = scoped_fact("a", None, "workspace", "h", -240);
        row.confidence = Some(0.8);

        let first = decay_target(&row, 0.8, now, &opts).expect("target for idle fact");
        row.properties
            .as_mut()
            .and_then(Value::as_object_mut)
            .expect("properties object")
            .insert("base_confidence".to_string(), json!(0.8));
        let second = decay_target(&row, first, now, &opts).expect("target stays computable");

        assert!((first - second).abs() <= EPSILON);
        assert!(first < 0.8);
    }

    #[test]
    fn decay_respects_floor_and_counts_at_floor() {
        // Pins: stale low-confidence facts decay to the configured floor, never below it.
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 0, 0, 0).unwrap();
        let opts = ConsolidationOptions {
            decay_idle_days: 30,
            decay_half_life_days: 90.0,
            decay_floor: 0.25,
        };
        let mut row = scoped_fact("a", None, "workspace", "h", -720);
        row.confidence = Some(0.5);

        let target = decay_target(&row, 0.5, now, &opts).expect("target for idle fact");

        assert_eq!(target, 0.25);
    }

    #[test]
    fn contradiction_sweep_newest_object_wins_deterministically() {
        // Pins: contradiction sweep keeper is newest valid_from with UID tie-break.
        let old = contradiction_fact("00000000-0000-8000-8000-000000000003", "team-a", 0);
        let newest_high_uid =
            contradiction_fact("00000000-0000-8000-8000-000000000004", "team-b", 10);
        let newest_low_uid =
            contradiction_fact("00000000-0000-8000-8000-000000000002", "team-c", 10);

        let groups = contradiction_groups(&[old, newest_high_uid, newest_low_uid]);

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0][0].uid,
            uuid("00000000-0000-8000-8000-000000000002")
        );
    }

    fn uids(rows: &[LifecycleNodeRow]) -> Vec<Uuid> {
        rows.iter().map(|row| row.uid).collect()
    }

    fn scoped_fact(
        uid_suffix: &str,
        user_id: Option<&str>,
        scope: &str,
        fact_hash: &str,
        day_offset: i64,
    ) -> LifecycleNodeRow {
        fact(FactSpec {
            uid_suffix,
            user_id,
            scope,
            fact_hash,
            subject: "s",
            predicate: "p",
            object: "o",
            day_offset,
        })
    }

    fn contradiction_fact(uid_suffix: &str, object: &str, day_offset: i64) -> LifecycleNodeRow {
        fact(FactSpec {
            uid_suffix,
            user_id: None,
            scope: "workspace",
            fact_hash: uid_suffix,
            subject: "service",
            predicate: "owner",
            object,
            day_offset,
        })
    }

    struct FactSpec<'a> {
        uid_suffix: &'a str,
        user_id: Option<&'a str>,
        scope: &'a str,
        fact_hash: &'a str,
        subject: &'a str,
        predicate: &'a str,
        object: &'a str,
        day_offset: i64,
    }

    fn fact(spec: FactSpec<'_>) -> LifecycleNodeRow {
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        LifecycleNodeRow {
            uid: uuid(spec.uid_suffix),
            workspace_id: Some("ws".to_string()),
            user_id: spec.user_id.map(ToOwned::to_owned),
            scope: spec.scope.to_string(),
            name: spec.subject.to_string(),
            pii_class: PiiClass::None,
            confidence: Some(0.9),
            valid_from: base + Duration::days(spec.day_offset),
            valid_to: None,
            properties: Some(json!({
                "fact_hash": spec.fact_hash,
                "subject": spec.subject,
                "predicate": spec.predicate,
                "object": spec.object,
            })),
            last_accessed_at: base + Duration::days(spec.day_offset),
            has_embedding: false,
            cached_object: None,
        }
    }

    fn uuid(value: &str) -> Uuid {
        if value.len() == 1 {
            let byte = value.as_bytes()[0];
            return Uuid::from_u128(byte as u128);
        }
        Uuid::parse_str(value).expect("test uuid should parse")
    }
}
