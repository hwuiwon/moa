//! Consolidation pass logic shared by workflows and eval runners.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use moa_core::RlsContext;
use moa_core::{
    ContactId, MemoryDigestConfig, StoragePartitionId, TenantId, traits::EmbeddingProvider,
};
use moa_memory_graph::{
    ExistingSupersessionIntent, GraphError, NodeEmbeddingIntent, NodeExpiryIntent, NodeIndexRow,
    NodeLabel, NodePropertyUpdateIntent, PiiClass, PostgresGraphStore,
};
use moa_memory_ingest::{EntityMergeVerifier, IngestError, normalize_entity_name};
use moa_memory_vector::VectorStoreFactory;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::digest::{DigestStats, rebuild_storage_digests};

/// Invalidation reason written when idle floor-bound facts are expired.
pub const EXPIRED_IDLE_REASON: &str = "expired_idle";

const CONSOLIDATION_ACTOR: &str = "consolidation";
const CONSOLIDATION_ACTOR_KIND: &str = "system";
const ACTIVE_ROWS_PAGE_SIZE: i64 = 1000;
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
    /// The entity merge verifier failed to adjudicate a candidate pair.
    #[error("entity merge verifier: {0}")]
    Verifier(#[from] IngestError),
    /// Stored graph row was malformed.
    #[error("invalid graph row: {0}")]
    InvalidRow(String),
}

/// Tuning options for confidence decay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationOptions {
    /// Number of idle days before confidence begins to decay.
    pub decay_idle_days: i64,
    /// Half-life applied to idle facts after the anchor confidence is captured.
    pub decay_half_life_days: f64,
    /// Minimum confidence retained by decay.
    pub decay_floor: f64,
    /// Idle days after which floor-bound facts are closed; `0` disables expiry.
    #[serde(default = "default_expire_idle_days")]
    pub expire_idle_days: i64,
    /// Standing digest rebuild configuration.
    #[serde(default)]
    pub digest: MemoryDigestConfig,
}

/// Default idle window before floor-bound facts expire.
fn default_expire_idle_days() -> i64 {
    180
}

impl Default for ConsolidationOptions {
    fn default() -> Self {
        Self {
            decay_idle_days: 30,
            decay_half_life_days: 180.0,
            decay_floor: 0.1,
            expire_idle_days: default_expire_idle_days(),
            digest: MemoryDigestConfig::default(),
        }
    }
}

/// Serializable outcome for one tenant consolidation pass.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationOutcome {
    /// Duplicate fact nodes invalidated into a canonical active node.
    pub merged: u64,
    /// Facts whose confidence was lowered during this pass.
    pub decayed: u64,
    /// Active facts whose computed confidence is at the configured floor.
    pub at_floor: u64,
    /// Floor-bound idle facts closed by bitemporal invalidation.
    #[serde(default)]
    pub expired_idle: u64,
    /// Older contradictory facts superseded by the newest fact.
    pub contradiction_supersessions: u64,
    /// Entity nodes that received missing embeddings.
    pub entity_embeddings_backfilled: u64,
    /// Alias mentions promoted from edge properties to entity node aliases.
    pub aliases_promoted: u64,
    /// Exact-duplicate groups that still have more than one active node after merge.
    pub duplicates_remaining: u64,
    /// Digest rows rebuilt or inserted.
    #[serde(default)]
    pub digests_rebuilt: u64,
    /// Digest rows skipped because they are fresher than the configured interval.
    #[serde(default)]
    pub digests_skipped_fresh: u64,
}

impl ConsolidationOutcome {
    /// Returns whether the pass performed no mutating work.
    #[must_use]
    pub fn has_no_work(&self) -> bool {
        self.merged == 0
            && self.decayed == 0
            && self.expired_idle == 0
            && self.contradiction_supersessions == 0
            && self.entity_embeddings_backfilled == 0
            && self.aliases_promoted == 0
            && self.digests_rebuilt == 0
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

/// Outcome for idle-fact expiry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiryStats {
    /// Floor-bound idle facts closed by bitemporal invalidation.
    pub expired_idle: u64,
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

/// Tuning for embedding-blocked entity resolution.
///
/// Blocking proposes near-duplicate candidate pairs by pgvector cosine distance;
/// the structural gate and the [`EntityMergeVerifier`] then decide which pairs
/// actually merge. Defaults follow the retrieval plan: a 0.15 cosine-distance
/// ceiling and at most five nearest neighbours probed per entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityResolutionOptions {
    /// Maximum cosine distance (`1 - cosine similarity`) for a proposed pair.
    pub cosine_distance_threshold: f64,
    /// Nearest neighbours probed per entity before the structural gate.
    pub candidates_per_entity: i64,
}

impl Default for EntityResolutionOptions {
    fn default() -> Self {
        Self {
            cosine_distance_threshold: 0.15,
            candidates_per_entity: 5,
        }
    }
}

/// Outcome for the embedding-blocked entity-resolution pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityResolutionStats {
    /// Structurally-gated candidate pairs that reached the merge verifier.
    pub pairs_adjudicated: u64,
    /// Newer entities superseded into an older canonical entity.
    pub entities_merged: u64,
}

/// Runs all v1 consolidation operations for one tenant.
pub async fn consolidate_tenant(
    pool: &PgPool,
    tenant_id: TenantId,
    opts: ConsolidationOptions,
    now: DateTime<Utc>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) -> Result<ConsolidationOutcome> {
    // Load the tenant's active fact set once and share it across the exact-duplicate
    // merge and the contradiction sweep. Merge invalidates duplicate nodes, so their
    // uids are removed from the shared snapshot before the sweep runs; otherwise the
    // sweep could try to supersede into a node that merge already closed. Confidence
    // decay is a set-based UPDATE and the digest re-reads post-decay confidence, so
    // neither of those passes consumes the in-memory snapshot.
    let facts = active_fact_rows(pool, &tenant_id).await?;
    let (merge, merged_closed) = merge_duplicate_rows(pool, &facts, now).await?;
    let decay = decay_confidence(pool, &tenant_id, now, &opts).await?;
    let live_after_merge = facts
        .into_iter()
        .filter(|fact| !merged_closed.contains(&fact.uid))
        .collect::<Vec<_>>();
    let (sweep, _swept_closed) = sweep_contradiction_rows(pool, &live_after_merge, now).await?;
    // Expiry runs after decay and the sweep so it sees post-decay confidence and
    // does not close a fact another pass would have superseded this run.
    let expiry = expire_idle_facts(pool, &tenant_id, now, &opts).await?;
    let backfill = backfill_entities(pool, &tenant_id, embedder).await?;
    let storage_partition_id = storage_partition_id(&tenant_id);
    let digest = rebuild_storage_digests(pool, &storage_partition_id, now, &opts.digest).await?;

    Ok(ConsolidationOutcome {
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
    })
}

/// Merges active exact-duplicate facts by `(tenant, contact, scope, fact_hash)`.
pub async fn merge_duplicates(
    pool: &PgPool,
    tenant_id: &TenantId,
    now: DateTime<Utc>,
) -> Result<MergeStats> {
    let facts = active_fact_rows(pool, tenant_id).await?;
    Ok(merge_duplicate_rows(pool, &facts, now).await?.0)
}

/// Merges exact-duplicate facts from a preloaded active-fact snapshot.
///
/// Returns the merge statistics together with the node uids invalidated by the
/// merge, so a caller sharing the snapshot across passes can drop those rows
/// before running the contradiction sweep.
async fn merge_duplicate_rows(
    pool: &PgPool,
    facts: &[LifecycleNodeRow],
    now: DateTime<Utc>,
) -> Result<(MergeStats, BTreeSet<Uuid>)> {
    let mut merged = 0_u64;
    let mut closed = BTreeSet::new();

    for group in duplicate_groups(facts) {
        // Every row in a duplicate group shares the same `(tenant, contact, scope)`
        // ownership by construction, so the scoped store is built once per group
        // instead of once per invalidated node.
        let store = scoped_graph_for(pool, group[0].scope_context());
        let canonical = &group[0];
        for duplicate in group.iter().skip(1) {
            close_into_existing(
                &store,
                duplicate,
                canonical,
                duplicate.valid_from,
                "merged",
                now,
            )
            .await?;
            closed.insert(duplicate.uid);
            merged += 1;
        }
    }

    // Each returned group has more than one active node and is collapsed to its
    // single canonical node above, so no exact-duplicate group survives the pass.
    Ok((
        MergeStats {
            merged,
            duplicates_remaining: 0,
        },
        closed,
    ))
}

/// Applies anchored confidence decay to idle active facts.
///
/// The decay is computed and written by a single set-based `UPDATE` (mirroring
/// the pattern in [`crate::quality`]) rather than reading the whole active-fact
/// set and issuing one write per node. The SQL reproduces [`decay_target`]
/// exactly: it anchors to the stored `base_confidence` (falling back to the live
/// confidence), applies the half-life decay over whole idle-days, clamps to the
/// floor, and only rewrites rows whose confidence actually changes. The pass
/// bumps the partition changelog version once when it writes anything, matching
/// the quality-scoring maintenance path.
pub async fn decay_confidence(
    pool: &PgPool,
    tenant_id: &TenantId,
    now: DateTime<Utc>,
    opts: &ConsolidationOptions,
) -> Result<DecayStats> {
    // `decay_target` bails out for a non-positive or non-finite half-life; skip the
    // whole set-based pass in that case so no row is rewritten.
    if opts.decay_half_life_days <= 0.0 || !opts.decay_half_life_days.is_finite() {
        return Ok(DecayStats::default());
    }
    let storage_partition_id = storage_partition_id(tenant_id).to_string();

    let row = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT node.uid,
                   node.confidence AS current_conf,
                   node.properties_summary,
                   COALESCE(
                       (node.properties_summary->>'base_confidence')::double precision,
                       node.confidence
                   ) AS base_conf,
                   GREATEST(
                       TRUNC(EXTRACT(EPOCH FROM ($2::timestamptz - node.last_accessed_at))),
                       0
                   )::double precision / 86400.0 AS idle_days
            FROM moa.node_index AS node
            WHERE node.tenant_id = $1
              AND node.label = 'Fact'
              AND node.valid_to IS NULL
              AND node.confidence IS NOT NULL
              AND EXTRACT(EPOCH FROM ($2::timestamptz - node.last_accessed_at))
                  >= ($3::double precision * 86400.0)
        ),
        computed AS (
            SELECT uid,
                   current_conf,
                   properties_summary,
                   LEAST(
                       GREATEST(
                           GREATEST(base_conf * power(0.5::double precision, idle_days / $4), $5),
                           0.0
                       ),
                       1.0
                   ) AS target
            FROM candidates
        ),
        to_update AS (
            SELECT uid, current_conf, properties_summary, target
            FROM computed
            WHERE ABS(target - current_conf) > $6
        ),
        updated AS (
            UPDATE moa.node_index AS node
            SET confidence = tu.target,
                properties_summary = jsonb_set(
                    CASE
                        WHEN node.properties_summary ? 'base_confidence'
                            THEN COALESCE(node.properties_summary, '{}'::jsonb)
                        ELSE jsonb_set(
                            COALESCE(node.properties_summary, '{}'::jsonb),
                            '{base_confidence}',
                            to_jsonb(tu.current_conf)
                        )
                    END,
                    '{confidence}',
                    to_jsonb(tu.target)
                )
            FROM to_update AS tu
            WHERE node.uid = tu.uid
            RETURNING node.uid
        ),
        bumped AS (
            INSERT INTO moa.storage_partition_state (storage_partition_id, changelog_version)
            SELECT $7, 1
            WHERE EXISTS (SELECT 1 FROM updated)
            ON CONFLICT (storage_partition_id) DO UPDATE
                SET changelog_version = moa.storage_partition_state.changelog_version + 1,
                    updated_at = now()
            RETURNING 1
        )
        SELECT
            (SELECT COUNT(*) FROM updated)::bigint AS decayed,
            (SELECT COUNT(*) FROM computed WHERE ABS(target - $5) <= $6)::bigint AS at_floor
        "#,
    )
    .bind(tenant_id.0)
    .bind(now)
    .bind(opts.decay_idle_days as f64)
    .bind(opts.decay_half_life_days)
    .bind(opts.decay_floor)
    .bind(EPSILON)
    .bind(storage_partition_id)
    .fetch_one(pool)
    .await?;

    let decayed = u64::try_from(row.try_get::<i64, _>("decayed")?)
        .map_err(|_| Error::InvalidRow("negative decayed count".to_string()))?;
    let at_floor = u64::try_from(row.try_get::<i64, _>("at_floor")?)
        .map_err(|_| Error::InvalidRow("negative at_floor count".to_string()))?;
    Ok(DecayStats { decayed, at_floor })
}

/// Closes floor-bound facts that have been idle past the expiry window.
///
/// A fact expires only when decay has already bottomed it out at the configured
/// floor AND nothing touched it (retrieval access, reinforcement) for
/// `expire_idle_days`. The close is a bitemporal invalidation with reason
/// `expired_idle` — history and as-of reads keep working — so the pass bounds
/// the active retrieval set without destroying anything. `expire_idle_days <= 0`
/// disables the pass. Rerunning at the same `now` is a no-op because closed
/// rows leave the candidate set.
pub async fn expire_idle_facts(
    pool: &PgPool,
    tenant_id: &TenantId,
    now: DateTime<Utc>,
    opts: &ConsolidationOptions,
) -> Result<ExpiryStats> {
    if opts.expire_idle_days <= 0 {
        return Ok(ExpiryStats::default());
    }

    let candidates = sqlx::query(
        r#"
        SELECT uid, contact_id
        FROM moa.node_index
        WHERE tenant_id = $1
          AND label = 'Fact'
          AND valid_to IS NULL
          AND confidence IS NOT NULL
          AND ABS(confidence - $2) <= $3
          AND EXTRACT(EPOCH FROM ($4::timestamptz - last_accessed_at))
              >= ($5::double precision * 86400.0)
        ORDER BY uid
        "#,
    )
    .bind(tenant_id.0)
    .bind(opts.decay_floor)
    .bind(EPSILON)
    .bind(now)
    .bind(opts.expire_idle_days as f64)
    .fetch_all(pool)
    .await?;

    // Per-node closes keep the per-node changelog record and vector delete
    // that expiry shares with manual invalidation. Candidate counts are small
    // after the first pass (only newly floor-bound idle facts qualify); batch
    // the closes per scope if scheduled consolidation ever shows this loop in
    // its latency profile.
    let mut stores: BTreeMap<Option<Uuid>, PostgresGraphStore> = BTreeMap::new();
    let mut expired_idle = 0_u64;
    for candidate in candidates {
        let uid: Uuid = candidate.try_get("uid")?;
        let contact_id: Option<Uuid> = candidate.try_get("contact_id")?;
        let scope = match contact_id {
            Some(contact_id) => RlsContext::contact(*tenant_id, ContactId(contact_id)),
            None => RlsContext::tenant(*tenant_id),
        };
        let store = stores
            .entry(contact_id)
            .or_insert_with(|| scoped_graph_for(pool, scope));
        let closed = store
            .expire_node(NodeExpiryIntent {
                uid,
                valid_to: now,
                invalidated_at: now,
                reason: EXPIRED_IDLE_REASON.to_string(),
                actor_id: CONSOLIDATION_ACTOR.to_string(),
                actor_kind: CONSOLIDATION_ACTOR_KIND.to_string(),
            })
            .await?;
        if closed {
            expired_idle += 1;
        }
    }

    Ok(ExpiryStats { expired_idle })
}

/// Supersedes older active contradictory facts using a deterministic newest-wins policy.
pub async fn sweep_contradictions(
    pool: &PgPool,
    tenant_id: &TenantId,
    now: DateTime<Utc>,
) -> Result<SweepStats> {
    let facts = active_fact_rows(pool, tenant_id).await?;
    Ok(sweep_contradiction_rows(pool, &facts, now).await?.0)
}

/// Sweeps contradictory facts from a preloaded active-fact snapshot.
///
/// Returns the sweep statistics together with the node uids superseded by the
/// sweep, so callers sharing the snapshot can drop those rows from later passes.
async fn sweep_contradiction_rows(
    pool: &PgPool,
    facts: &[LifecycleNodeRow],
    now: DateTime<Utc>,
) -> Result<(SweepStats, BTreeSet<Uuid>)> {
    let mut supersessions = 0_u64;
    let mut closed = BTreeSet::new();

    for group in contradiction_groups(facts) {
        // Every row in a contradiction group shares the same `(tenant, contact, scope)`
        // ownership, so the scoped store is built once per group.
        let store = scoped_graph_for(pool, group[0].scope_context());
        let keeper = &group[0];
        for old in group.iter().skip(1) {
            let valid_to = if keeper.valid_from > old.valid_from {
                keeper.valid_from
            } else {
                old.valid_from
            };
            close_into_existing(&store, old, keeper, valid_to, "contradiction_sweep", now).await?;
            closed.insert(old.uid);
            supersessions += 1;
        }
    }

    Ok((
        SweepStats {
            contradiction_supersessions: supersessions,
        },
        closed,
    ))
}

/// Backfills missing entity embeddings and promotes edge alias mentions to entity properties.
pub async fn backfill_entities(
    pool: &PgPool,
    tenant_id: &TenantId,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) -> Result<BackfillStats> {
    let entities = active_entity_rows(pool, tenant_id).await?;
    let mut embeddings = 0_u64;
    let mut aliases = 0_u64;
    // One scoped graph store per distinct `(tenant, contact)` scope, reused across
    // the embedding and alias writes instead of rebuilt for every node.
    let mut stores: BTreeMap<Option<Uuid>, PostgresGraphStore> = BTreeMap::new();

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
                scoped_graph_cached(&mut stores, pool, entity)
                    .upsert_node_embedding(NodeEmbeddingIntent {
                        uid: entity.uid,
                        embedding: vector,
                        embedding_model: embedder.model_id().to_string(),
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
            tenant_id = %tenant_id,
            "entity embedding backfill skipped because no embedder was provided"
        );
    }

    let storage_partition_id = storage_partition_id(tenant_id);
    let mut aliases_by_entity =
        edge_aliases_for_entities(pool, &storage_partition_id, &entities).await?;
    for entity in &entities {
        let promoted = promote_aliases_for_entity(
            &mut stores,
            pool,
            entity,
            aliases_by_entity.remove(&entity.uid).unwrap_or_default(),
        )
        .await?;
        aliases += promoted;
    }

    Ok(BackfillStats {
        entity_embeddings_backfilled: embeddings,
        aliases_promoted: aliases,
    })
}

/// Resolves near-duplicate entities by embedding blocking, a structural gate,
/// and a merge verifier, superseding accepted duplicates into a canonical node.
///
/// This is the incremental, off-hot-path entity-resolution pass. It runs in
/// three stages so paraphrase-level duplicates that name normalization misses
/// still merge, without the over-merging that a pure embedding threshold causes:
///
/// 1. **Blocking.** A pgvector self-similarity join over active `Entity`
///    embeddings proposes the nearest [`EntityResolutionOptions::candidates_per_entity`]
///    neighbours (cosine distance below
///    [`EntityResolutionOptions::cosine_distance_threshold`]) for every entity
///    created at or after `since`. Only entities in the *same*
///    `(storage_partition_id, contact_id)` scope are ever paired, so blocking
///    never crosses tenant or contact ownership.
/// 2. **Structural gate.** A proposed pair only survives when its endpoints
///    share at least one active graph neighbour (`moa.edge_index`, `valid_to IS
///    NULL`). Pairs that fail the gate never reach the verifier — embedding
///    similarity alone is not allowed to trigger a merge.
/// 3. **Adjudication and merge.** Surviving pairs go through `verifier`. On a
///    yes, the newer entity is superseded into the older canonical one
///    (canonical = earliest `valid_from`, uid tie-break, matching the
///    exact-duplicate convention) through the node supersession write protocol,
///    which closes the superseded node's incident edges in the same
///    transaction. Entities are never deleted, so provenance is preserved.
///
/// `since` makes the pass incremental: only entities created at or after it are
/// probed, but they are matched against the full active entity set. Transitive
/// chains (`A`↔`B`↔`C`) collapse one hop per pass because a node already closed
/// earlier in the same run is skipped; the remainder resolves on the next run.
pub async fn resolve_entity_duplicates(
    pool: &PgPool,
    tenant_id: &TenantId,
    verifier: &dyn EntityMergeVerifier,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    opts: &EntityResolutionOptions,
) -> Result<EntityResolutionStats> {
    let pairs = block_entity_candidate_pairs(pool, tenant_id, since, opts).await?;
    if pairs.is_empty() {
        return Ok(EntityResolutionStats::default());
    }

    // Fetch the canonical (older) rows the verifier adjudicates against in one
    // batch instead of one query per pair.
    let canonical_uids = pairs
        .iter()
        .map(|pair| pair.canonical.uid)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let canonical_rows = active_entity_rows_by_uid(pool, &canonical_uids).await?;

    let mut stores: BTreeMap<Option<Uuid>, PostgresGraphStore> = BTreeMap::new();
    let mut closed = BTreeSet::new();
    let mut stats = EntityResolutionStats::default();

    for pair in pairs {
        // A node already superseded earlier in this pass cannot be re-closed and
        // must not become a canonical target; defer such transitive pairs.
        if closed.contains(&pair.canonical.uid) || closed.contains(&pair.duplicate.uid) {
            continue;
        }
        let Some(candidate_row) = canonical_rows.get(&pair.canonical.uid) else {
            continue;
        };
        stats.pairs_adjudicated += 1;
        if !verifier
            .should_merge(&pair.duplicate.name, candidate_row)
            .await?
        {
            continue;
        }

        let store = stores
            .entry(pair.contact_id)
            .or_insert_with(|| scoped_graph_for(pool, pair.scope_context(tenant_id)));
        store
            .close_existing_node_with_supersession(ExistingSupersessionIntent {
                old_uid: pair.duplicate.uid,
                replacement_uid: pair.canonical.uid,
                valid_to: pair.duplicate.valid_from,
                invalidated_at: now,
                reason: "entity_resolution".to_string(),
                actor_id: CONSOLIDATION_ACTOR.to_string(),
                actor_kind: CONSOLIDATION_ACTOR_KIND.to_string(),
            })
            .await?;
        closed.insert(pair.duplicate.uid);
        stats.entities_merged += 1;
    }

    Ok(stats)
}

/// One structurally-gated near-duplicate entity pair, canonical endpoint first.
#[derive(Debug, Clone)]
struct EntityCandidatePair {
    canonical: EntityCandidateEndpoint,
    duplicate: EntityCandidateEndpoint,
    contact_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
struct EntityCandidateEndpoint {
    uid: Uuid,
    name: String,
    valid_from: DateTime<Utc>,
}

impl EntityCandidatePair {
    fn scope_context(&self, tenant_id: &TenantId) -> RlsContext {
        match self.contact_id {
            Some(contact_id) => RlsContext::contact(*tenant_id, ContactId(contact_id)),
            None => RlsContext::tenant(*tenant_id),
        }
    }
}

/// Proposes near-duplicate entity pairs by embedding blocking and the
/// shared-active-neighbour structural gate.
///
/// The lateral self-join probes only entities created at or after `since`
/// against every active same-scope entity, keeping the nearest neighbours under
/// the cosine-distance ceiling. The `EXISTS` clause enforces the structural gate
/// in SQL so pairs without a shared active neighbour never leave Postgres.
/// Endpoints are ordered so the canonical entity (earliest `valid_from`, uid
/// tie-break) is first, and each unordered pair is emitted once.
async fn block_entity_candidate_pairs(
    pool: &PgPool,
    tenant_id: &TenantId,
    since: DateTime<Utc>,
    opts: &EntityResolutionOptions,
) -> Result<Vec<EntityCandidatePair>> {
    let rows = sqlx::query(
        r#"
        WITH probe AS (
            SELECT node.uid,
                   node.name,
                   node.valid_from,
                   node.storage_partition_id,
                   node.contact_id,
                   embedding.embedding
            FROM moa.node_index AS node
            JOIN moa.embeddings AS embedding
              ON embedding.uid = node.uid
             AND embedding.valid_to IS NULL
            WHERE node.tenant_id = $1
              AND node.label = 'Entity'
              AND node.valid_to IS NULL
              AND node.valid_from >= $2
        )
        SELECT probe.uid            AS probe_uid,
               probe.name           AS probe_name,
               probe.valid_from     AS probe_valid_from,
               probe.contact_id     AS contact_id,
               cand.uid             AS cand_uid,
               cand.name            AS cand_name,
               cand.valid_from      AS cand_valid_from
        FROM probe
        JOIN LATERAL (
            SELECT other.uid,
                   other.name,
                   other.valid_from,
                   probe.embedding <=> other_embedding.embedding AS distance
            FROM moa.node_index AS other
            JOIN moa.embeddings AS other_embedding
              ON other_embedding.uid = other.uid
             AND other_embedding.valid_to IS NULL
            WHERE other.tenant_id = $1
              AND other.label = 'Entity'
              AND other.valid_to IS NULL
              AND other.uid <> probe.uid
              AND other.storage_partition_id IS NOT DISTINCT FROM probe.storage_partition_id
              AND other.contact_id IS NOT DISTINCT FROM probe.contact_id
              AND (probe.embedding <=> other_embedding.embedding) < $3
            ORDER BY probe.embedding <=> other_embedding.embedding, other.uid
            LIMIT $4
        ) AS cand ON TRUE
        WHERE EXISTS (
            SELECT 1
            FROM (
                SELECT end_uid AS nbr FROM moa.edge_index
                 WHERE start_uid = probe.uid AND valid_to IS NULL
                UNION
                SELECT start_uid AS nbr FROM moa.edge_index
                 WHERE end_uid = probe.uid AND valid_to IS NULL
            ) AS probe_nbr
            JOIN (
                SELECT end_uid AS nbr FROM moa.edge_index
                 WHERE start_uid = cand.uid AND valid_to IS NULL
                UNION
                SELECT start_uid AS nbr FROM moa.edge_index
                 WHERE end_uid = cand.uid AND valid_to IS NULL
            ) AS cand_nbr ON probe_nbr.nbr = cand_nbr.nbr
            WHERE probe_nbr.nbr <> probe.uid
              AND probe_nbr.nbr <> cand.uid
        )
        "#,
    )
    .bind(tenant_id.0)
    .bind(since)
    .bind(opts.cosine_distance_threshold)
    .bind(opts.candidates_per_entity)
    .fetch_all(pool)
    .await?;

    // The lateral join can surface a pair from both directions (each endpoint
    // probing the other); collapse to one canonical-first pair per uid set.
    let mut pairs: BTreeMap<(Uuid, Uuid), EntityCandidatePair> = BTreeMap::new();
    for row in rows {
        let probe = EntityCandidateEndpoint {
            uid: row.try_get("probe_uid")?,
            name: row.try_get("probe_name")?,
            valid_from: row.try_get("probe_valid_from")?,
        };
        let cand = EntityCandidateEndpoint {
            uid: row.try_get("cand_uid")?,
            name: row.try_get("cand_name")?,
            valid_from: row.try_get("cand_valid_from")?,
        };
        let contact_id: Option<Uuid> = row.try_get("contact_id")?;
        let (canonical, duplicate) = if (probe.valid_from, probe.uid) <= (cand.valid_from, cand.uid)
        {
            (probe, cand)
        } else {
            (cand, probe)
        };
        pairs.insert(
            (canonical.uid, duplicate.uid),
            EntityCandidatePair {
                canonical,
                duplicate,
                contact_id,
            },
        );
    }

    Ok(pairs.into_values().collect())
}

/// Loads active `Entity` index rows for the merge verifier keyed by uid.
async fn active_entity_rows_by_uid(
    pool: &PgPool,
    uids: &[Uuid],
) -> Result<BTreeMap<Uuid, NodeIndexRow>> {
    if uids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at,
               COALESCE(quality_score, 0.5) AS quality_score
        FROM moa.node_index
        WHERE uid = ANY($1)
          AND label = 'Entity'
          AND valid_to IS NULL
        "#,
    )
    .bind(uids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| (row.uid, row)).collect())
}

/// Rebuilds deterministic standing digest rows for one tenant.
pub async fn rebuild_digests(
    pool: &PgPool,
    tenant_id: &TenantId,
    now: DateTime<Utc>,
    config: &MemoryDigestConfig,
) -> Result<DigestStats> {
    let storage_partition_id = storage_partition_id(tenant_id);
    rebuild_storage_digests(pool, &storage_partition_id, now, config).await
}

/// Tenant consolidation cursor captured from the partition registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TenantConsolidationCursor {
    /// Tenant whose graph changed after the last successful consolidation.
    pub tenant_id: TenantId,
    /// Changelog version observed when the tenant was selected.
    pub changelog_version: i64,
}

/// Returns the tenants whose graph changed since their last consolidation.
///
/// This is the incremental cursor that lets periodic maintenance skip idle
/// tenants. Each tenant partition tracks two watermarks in
/// `moa.storage_partition_state`: `changelog_version`, bumped on every graph
/// write (and by set-based maintenance), and `consolidated_changelog_version`,
/// advanced by [`advance_consolidation_watermark`] after a successful
/// consolidation pass. A tenant is returned only when its live version has moved
/// past the recorded consolidation watermark, so tenants with no new graph
/// activity short-circuit without dispatching a workflow. The registry holds
/// one row per tenant partition, so this replaces a `SELECT DISTINCT` scan over
/// the entire node index. Results are ordered by tenant id for deterministic
/// dispatch.
pub async fn tenants_needing_consolidation(
    pool: &PgPool,
) -> Result<Vec<TenantConsolidationCursor>> {
    let rows = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        SELECT tenant_id, changelog_version
        FROM moa.storage_partition_state
        WHERE tenant_id IS NOT NULL
          AND changelog_version > consolidated_changelog_version
        ORDER BY tenant_id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(tenant_id, changelog_version)| TenantConsolidationCursor {
            tenant_id: TenantId::from(tenant_id),
            changelog_version,
        })
        .collect())
}

/// Returns the current changelog version for one tenant partition.
pub async fn tenant_changelog_version(pool: &PgPool, tenant_id: &TenantId) -> Result<i64> {
    sqlx::query_scalar(
        r#"
        SELECT changelog_version
        FROM moa.storage_partition_state
        WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id.0)
    .fetch_optional(pool)
    .await
    .map(|version| version.unwrap_or_default())
    .map_err(Into::into)
}

/// Advances tenant consolidation watermarks to observed changelog versions.
///
/// Callers invoke this only after a consolidation pass succeeds. The target
/// version comes from the tenant cursor observed before the workflow started,
/// not from the live registry row at update time, so writes that arrive while a
/// workflow is running remain pending for the next maintenance tick.
pub async fn advance_consolidation_watermark(
    pool: &PgPool,
    cursors: &[TenantConsolidationCursor],
) -> Result<()> {
    if cursors.is_empty() {
        return Ok(());
    }
    let tenant_uuids = cursors
        .iter()
        .map(|cursor| cursor.tenant_id.0)
        .collect::<Vec<_>>();
    let changelog_versions = cursors
        .iter()
        .map(|cursor| cursor.changelog_version)
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        UPDATE moa.storage_partition_state AS state
        SET consolidated_changelog_version = GREATEST(
                state.consolidated_changelog_version,
                LEAST(state.changelog_version, cursor.changelog_version)
            ),
            updated_at = now()
        FROM UNNEST($1::uuid[], $2::bigint[]) AS cursor(tenant_id, changelog_version)
        WHERE state.tenant_id = cursor.tenant_id
        "#,
    )
    .bind(&tenant_uuids)
    .bind(&changelog_versions)
    .execute(pool)
    .await?;
    Ok(())
}

async fn close_into_existing(
    store: &PostgresGraphStore,
    old: &LifecycleNodeRow,
    replacement: &LifecycleNodeRow,
    valid_to: DateTime<Utc>,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    store
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
        // Duplicates are keyed by normalized fact content, not the full
        // fact_hash: the hash also covers the free-text summary, so the same
        // fact restated in different words ("still true that X depends on Y")
        // would never merge. Subject/predicate/object is the fact identity used
        // by final-selection dedupe and probe equivalence; consolidation
        // follows the same rule. Update-era facts keep distinct objects, so
        // bitemporal families never collapse here.
        let (Some(subject), Some(predicate), Some(object)) = (
            row.property_text("subject"),
            row.property_text("predicate"),
            row.property_text("object"),
        ) else {
            continue;
        };
        groups
            .entry(DuplicateKey {
                tenant_id: row.tenant_id,
                contact_id: row.contact_id,
                scope: row.scope.clone(),
                subject: moa_memory_types::normalize_fact_component(&subject),
                predicate: moa_memory_types::normalize_fact_component(&predicate),
                object: moa_memory_types::normalize_fact_component(&object),
            })
            .or_default()
            .push(row.clone());
    }

    // `into_values()` already yields groups in ascending `DuplicateKey` order, so
    // no further sort across groups is required for deterministic output.
    groups
        .into_values()
        .filter(|group| group.len() > 1)
        .map(|mut group| {
            group.sort_by_key(|row| (row.valid_from, row.uid));
            group
        })
        .collect()
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
        if !is_sweepable_contradiction_predicate(&predicate) {
            continue;
        }
        groups
            .entry(ContradictionKey {
                tenant_id: row.tenant_id,
                contact_id: row.contact_id,
                scope: row.scope.clone(),
                subject,
                predicate,
            })
            .or_default()
            .push(row.clone().with_cached_object(object));
    }

    // `into_values()` already yields groups in ascending `ContradictionKey` order,
    // so no further sort across groups is required for deterministic output.
    groups
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
        .collect()
}

fn is_sweepable_contradiction_predicate(predicate: &str) -> bool {
    let normalized = predicate.trim().to_ascii_lowercase().replace('_', " ");
    matches!(
        normalized.as_str(),
        "cache backend conflict" | "deploy target" | "on call primary"
    )
}

/// Computes the anchored confidence-decay target for a single fact.
///
/// This is the reference implementation the set-based [`decay_confidence`] pass
/// reproduces in SQL: given the anchor `base_confidence` (the fact's stored
/// `base_confidence`, or its live confidence when no anchor has been recorded)
/// and the fact's `last_accessed_at`, it applies half-life decay over whole idle
/// days and clamps to the configured floor. It returns `None` when the fact is
/// too fresh to decay or the half-life is not a usable positive, finite value,
/// meaning the fact's confidence is left untouched.
#[must_use]
pub fn decay_target_confidence(
    base_confidence: f64,
    last_accessed_at: DateTime<Utc>,
    now: DateTime<Utc>,
    opts: &ConsolidationOptions,
) -> Option<f64> {
    let idle = now.signed_duration_since(last_accessed_at);
    if idle < Duration::days(opts.decay_idle_days) {
        return None;
    }
    if opts.decay_half_life_days <= 0.0 || !opts.decay_half_life_days.is_finite() {
        return None;
    }
    let idle_days = idle.num_seconds().max(0) as f64 / 86_400.0;
    Some(
        (base_confidence * 0.5_f64.powf(idle_days / opts.decay_half_life_days))
            .max(opts.decay_floor)
            .clamp(0.0, 1.0),
    )
}

#[cfg(test)]
fn decay_target(
    fact: &LifecycleNodeRow,
    current: f64,
    now: DateTime<Utc>,
    opts: &ConsolidationOptions,
) -> Option<f64> {
    let base = fact
        .properties
        .as_ref()
        .and_then(|value| value.get("base_confidence"))
        .and_then(Value::as_f64)
        .unwrap_or(current);
    decay_target_confidence(base, fact.last_accessed_at, now, opts)
}

async fn promote_aliases_for_entity(
    stores: &mut BTreeMap<Option<Uuid>, PostgresGraphStore>,
    pool: &PgPool,
    entity: &LifecycleNodeRow,
    aliases: BTreeSet<String>,
) -> Result<u64> {
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
    scoped_graph_cached(stores, pool, entity)
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

async fn edge_aliases_for_entities(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    entities: &[LifecycleNodeRow],
) -> Result<BTreeMap<Uuid, BTreeSet<String>>> {
    if entities.is_empty() {
        return Ok(BTreeMap::new());
    }
    let entity_uids = entities.iter().map(|entity| entity.uid).collect::<Vec<_>>();
    // Derive alias mentions from the live edge table rather than scanning the
    // append-only `graph_changelog` with non-indexable JSON expressions. Edge
    // creation writes `properties` verbatim into `moa.edge_index` (the same value
    // it records under `payload->'after'` in the changelog), and the
    // `(storage_partition_id, start_uid)` / `(storage_partition_id, end_uid)`
    // indexes make the lookup selective. Reading live edges also excludes edges
    // that were later removed, which is the desired promotion semantics.
    let rows = sqlx::query(
        r#"
        SELECT edge.start_uid AS entity_uid,
               edge.properties->>'alias_mention' AS alias
        FROM moa.edge_index AS edge
        WHERE edge.storage_partition_id = $1
          AND edge.start_uid = ANY($2)
          AND edge.valid_to IS NULL
          AND edge.properties ? 'alias_mention'
        UNION
        SELECT edge.end_uid AS entity_uid,
               edge.properties->>'alias_mention' AS alias
        FROM moa.edge_index AS edge
        WHERE edge.storage_partition_id = $1
          AND edge.end_uid = ANY($2)
          AND edge.valid_to IS NULL
          AND edge.properties ? 'alias_mention'
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(&entity_uids)
    .fetch_all(pool)
    .await?;

    let mut aliases_by_entity = BTreeMap::<Uuid, BTreeSet<String>>::new();
    for row in rows {
        let entity_uid = row.try_get::<Uuid, _>("entity_uid")?;
        let Some(alias) = row.try_get::<Option<String>, _>("alias")? else {
            continue;
        };
        let alias = alias.trim().to_string();
        if !alias.is_empty() {
            aliases_by_entity
                .entry(entity_uid)
                .or_default()
                .insert(alias);
        }
    }
    Ok(aliases_by_entity)
}

async fn active_fact_rows(pool: &PgPool, tenant_id: &TenantId) -> Result<Vec<LifecycleNodeRow>> {
    active_rows(pool, tenant_id, NodeLabel::Fact, false).await
}

async fn active_entity_rows(pool: &PgPool, tenant_id: &TenantId) -> Result<Vec<LifecycleNodeRow>> {
    active_rows(pool, tenant_id, NodeLabel::Entity, true).await
}

async fn active_rows(
    pool: &PgPool,
    tenant_id: &TenantId,
    label: NodeLabel,
    include_embedding_state: bool,
) -> Result<Vec<LifecycleNodeRow>> {
    let mut rows = Vec::new();
    let mut cursor_valid_from: Option<DateTime<Utc>> = None;
    let mut cursor_uid: Option<Uuid> = None;

    loop {
        let batch = sqlx::query(
            r#"
            SELECT node.uid,
                   node.user_id,
                   node.tenant_id,
                   node.contact_id,
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
            WHERE node.tenant_id = $1
              AND node.label = $2
              AND node.valid_to IS NULL
              AND ($3::timestamptz IS NULL OR (node.valid_from, node.uid) > ($3, $4::uuid))
            ORDER BY node.valid_from ASC, node.uid ASC
            LIMIT $5
            "#,
        )
        .bind(tenant_id.0)
        .bind(label.as_str())
        .bind(cursor_valid_from)
        .bind(cursor_uid)
        .bind(ACTIVE_ROWS_PAGE_SIZE)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| lifecycle_row_from_sql(row, include_embedding_state))
        .collect::<Result<Vec<_>>>()?;

        let batch_len = batch.len();
        if let Some(last) = batch.last() {
            cursor_valid_from = Some(last.valid_from);
            cursor_uid = Some(last.uid);
        }
        rows.extend(batch);

        if batch_len < ACTIVE_ROWS_PAGE_SIZE as usize {
            break;
        }
    }

    Ok(rows)
}

fn storage_partition_id(tenant_id: &TenantId) -> StoragePartitionId {
    StoragePartitionId::for_tenant(*tenant_id)
}

fn lifecycle_row_from_sql(
    row: sqlx::postgres::PgRow,
    include_embedding_state: bool,
) -> Result<LifecycleNodeRow> {
    let pii_class: String = row.try_get("pii_class")?;
    Ok(LifecycleNodeRow {
        uid: row.try_get("uid")?,
        user_id: row.try_get("user_id")?,
        tenant_id: row.try_get("tenant_id")?,
        contact_id: row.try_get("contact_id")?,
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

fn scoped_graph_for(pool: &PgPool, scope: RlsContext) -> PostgresGraphStore {
    let vector_backend = VectorStoreFactory::default().transactional_graph_backend(
        pool.clone(),
        scope.clone(),
        false,
    );
    PostgresGraphStore::scoped(pool.clone(), scope)
        .with_vector_store(vector_backend.vector_store())
        .with_vector_post_commit_sync(vector_backend.post_commit_sync())
}

/// Returns a scoped graph store for `row`, building and caching one store per
/// distinct `(tenant, contact)` scope so per-node loops stop reconstructing the
/// store (and its vector backend) on every write.
fn scoped_graph_cached<'a>(
    stores: &'a mut BTreeMap<Option<Uuid>, PostgresGraphStore>,
    pool: &PgPool,
    row: &LifecycleNodeRow,
) -> &'a PostgresGraphStore {
    stores
        .entry(row.contact_id)
        .or_insert_with(|| scoped_graph_for(pool, row.scope_context()))
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

#[derive(Debug, Clone, PartialEq)]
struct LifecycleNodeRow {
    uid: Uuid,
    user_id: Option<String>,
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
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
        crate::property_string(&self.properties, key)
    }

    fn properties_object(&self) -> serde_json::Map<String, Value> {
        self.properties
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    fn scope_context(&self) -> RlsContext {
        let tenant_id = TenantId::from(self.tenant_id);
        match self.contact_id {
            Some(contact_id) => RlsContext::contact(tenant_id, ContactId(contact_id)),
            None => RlsContext::tenant(tenant_id),
        }
    }

    fn with_cached_object(mut self, object: String) -> Self {
        self.cached_object = Some(object);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DuplicateKey {
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
    scope: String,
    subject: String,
    predicate: String,
    object: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ContradictionKey {
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
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
        // Pins: exact duplicate grouping never crosses tenant or contact ownership.
        let rows = vec![
            scoped_fact("a", Uuid::from_u128(0x100), None, "tenant", "hash-1", 0),
            scoped_fact("b", Uuid::from_u128(0x100), None, "tenant", "hash-1", 1),
            scoped_fact(
                "c",
                Uuid::from_u128(0x100),
                Some(Uuid::from_u128(0x101)),
                "contact",
                "hash-1",
                2,
            ),
            scoped_fact(
                "d",
                Uuid::from_u128(0x100),
                Some(Uuid::from_u128(0x101)),
                "contact",
                "hash-1",
                3,
            ),
            scoped_fact(
                "e",
                Uuid::from_u128(0x100),
                Some(Uuid::from_u128(0x102)),
                "contact",
                "hash-1",
                4,
            ),
            scoped_fact("f", Uuid::from_u128(0x200), None, "tenant", "hash-1", 5),
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
            Uuid::from_u128(0x100),
            None,
            "tenant",
            "h",
            0,
        );
        let uid_tiebreak = scoped_fact(
            "00000000-0000-8000-8000-000000000001",
            Uuid::from_u128(0x100),
            None,
            "tenant",
            "h",
            0,
        );
        let later = scoped_fact(
            "00000000-0000-8000-8000-000000000003",
            Uuid::from_u128(0x100),
            None,
            "tenant",
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
        let mut row = scoped_fact("a", Uuid::from_u128(0x100), None, "tenant", "h", -240);
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
            ..ConsolidationOptions::default()
        };
        let mut row = scoped_fact("a", Uuid::from_u128(0x100), None, "tenant", "h", -720);
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

    #[test]
    fn contradiction_sweep_skips_broad_preference_predicates() {
        // Pins: broad extraction verbs do not let unrelated user preferences supersede each other.
        let style = fact(FactSpec {
            uid_suffix: "00000000-0000-8000-8000-000000000001",
            tenant_id: Uuid::from_u128(0x100),
            contact_id: Some(Uuid::from_u128(0x101)),
            scope: "contact",
            fact_hash: "style",
            subject: "User 02",
            predicate: "switched to",
            object: "step-by-step checklists",
            day_offset: 1,
        });
        let contact = fact(FactSpec {
            uid_suffix: "00000000-0000-8000-8000-000000000002",
            tenant_id: Uuid::from_u128(0x100),
            contact_id: Some(Uuid::from_u128(0x101)),
            scope: "contact",
            fact_hash: "contact",
            subject: "User 02",
            predicate: "switched to",
            object: "[EMAIL_REDACTED]",
            day_offset: 2,
        });

        assert!(contradiction_groups(&[style, contact]).is_empty());
    }

    #[test]
    fn contradiction_sweep_skips_multi_value_dependency_and_owner_predicates() {
        // Pins: corpus dependency and owner facts are multi-valued evidence, not v1 contradictions.
        let dependency_a = fact(FactSpec {
            uid_suffix: "00000000-0000-8000-8000-000000000001",
            tenant_id: Uuid::from_u128(0x100),
            contact_id: Some(Uuid::from_u128(0x101)),
            scope: "contact",
            fact_hash: "dependency-a",
            subject: "checkout-service",
            predicate: "depends_on",
            object: "lib-auth",
            day_offset: 1,
        });
        let dependency_b = fact(FactSpec {
            uid_suffix: "00000000-0000-8000-8000-000000000002",
            tenant_id: Uuid::from_u128(0x100),
            contact_id: Some(Uuid::from_u128(0x101)),
            scope: "contact",
            fact_hash: "dependency-b",
            subject: "checkout-service",
            predicate: "depends_on",
            object: "lib-ledger",
            day_offset: 2,
        });
        let owner_a = fact(FactSpec {
            uid_suffix: "00000000-0000-8000-8000-000000000003",
            tenant_id: Uuid::from_u128(0x100),
            contact_id: None,
            scope: "tenant",
            fact_hash: "owner-a",
            subject: "lib-auth",
            predicate: "owned_by",
            object: "identity",
            day_offset: 1,
        });
        let owner_b = fact(FactSpec {
            uid_suffix: "00000000-0000-8000-8000-000000000004",
            tenant_id: Uuid::from_u128(0x100),
            contact_id: None,
            scope: "tenant",
            fact_hash: "owner-b",
            subject: "lib-auth",
            predicate: "owned_by",
            object: "platform",
            day_offset: 2,
        });

        assert!(contradiction_groups(&[dependency_a, dependency_b, owner_a, owner_b]).is_empty());
    }

    fn uids(rows: &[LifecycleNodeRow]) -> Vec<Uuid> {
        rows.iter().map(|row| row.uid).collect()
    }

    fn scoped_fact(
        uid_suffix: &str,
        tenant_id: Uuid,
        contact_id: Option<Uuid>,
        scope: &str,
        fact_hash: &str,
        day_offset: i64,
    ) -> LifecycleNodeRow {
        fact(FactSpec {
            uid_suffix,
            tenant_id,
            contact_id,
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
            tenant_id: Uuid::from_u128(0x100),
            contact_id: None,
            scope: "tenant",
            fact_hash: uid_suffix,
            subject: "service",
            predicate: "cache_backend_conflict",
            object,
            day_offset,
        })
    }

    struct FactSpec<'a> {
        uid_suffix: &'a str,
        tenant_id: Uuid,
        contact_id: Option<Uuid>,
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
            user_id: spec.contact_id.map(|contact_id| contact_id.to_string()),
            tenant_id: spec.tenant_id,
            contact_id: spec.contact_id,
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
