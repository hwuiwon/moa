//! Read-side implementation for the relational graph store.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use moa_crypto::{Ciphertext, DecryptionRequest, EncryptionContext};
use sqlx::{Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{
    Error, GraphExpansionHit, GraphStore, GraphTraversalDirection, GraphWalkScoring,
    PostgresGraphStore,
    edge::{EdgeLabel, EdgeWriteIntent},
    node::{
        NODE_INDEX_COLUMNS, NodeIndexRow, NodeLabel, NodeReinforcementIntent, NodeWriteIntent,
        SEALED_NODE_INDEX_EXTRA_COLUMNS, SealedNodeRow,
    },
    write::{SEALED_CONTENT_VERSION, SealedNodeContent},
};

impl PostgresGraphStore {
    /// Opens restricted/PHI content on a batch of freshly fetched rows.
    ///
    /// This is the single decryption boundary shared by every read method that
    /// projects node content (`get_node`, `bulk_get_nodes`, `neighbors`,
    /// `lookup_seeds`, `lookup_seeds_batch`): each fetch decodes into
    /// [`SealedNodeRow`] and funnels through here, so no read path can forget to
    /// decrypt. `expand_seeds` is exempt because it returns only uid/label/scoring
    /// (no sealed content); callers hydrate reached nodes through `bulk_get_nodes`,
    /// which decrypts. Rows with no sealed content pass through untouched.
    async fn decrypt_sealed_rows(
        &self,
        sealed: Vec<SealedNodeRow>,
    ) -> Result<Vec<NodeIndexRow>, Error> {
        let mut rows = sealed
            .into_iter()
            .map(Some)
            .collect::<Vec<Option<SealedNodeRow>>>();
        let mut groups: BTreeMap<(Uuid, Uuid), Vec<(usize, DecryptionRequest)>> = BTreeMap::new();

        for (index, sealed) in rows.iter().enumerate() {
            let Some(sealed) = sealed.as_ref() else {
                return Err(Error::InvalidSealedContent(
                    "decryption row slot was unexpectedly empty".to_string(),
                ));
            };
            if !sealed.row.pii_class.is_sealed() {
                if sealed.content_sealed.is_some() {
                    return Err(Error::InvalidSealedContent(format!(
                        "unsealed node {} unexpectedly carries sealed content",
                        sealed.row.uid
                    )));
                }
                continue;
            }
            let tenant_id = sealed.tenant_id.ok_or_else(|| {
                Error::InvalidSealedContent(format!(
                    "sealed node {} is missing tenant_id",
                    sealed.row.uid
                ))
            })?;
            let data_subject_id = sealed.data_subject_id.ok_or_else(|| {
                Error::InvalidSealedContent(format!(
                    "sealed node {} is missing data_subject_id",
                    sealed.row.uid
                ))
            })?;
            let sealed_bytes = sealed.content_sealed.as_deref().ok_or_else(|| {
                Error::InvalidSealedContent(format!(
                    "restricted node {} has not been sealed",
                    sealed.row.uid
                ))
            })?;
            let context = EncryptionContext::new(
                tenant_id,
                data_subject_id,
                sealed.row.uid.to_string(),
                sealed.row.pii_class.as_str(),
            );
            groups
                .entry((tenant_id, data_subject_id))
                .or_default()
                .push((
                    index,
                    DecryptionRequest::new(Ciphertext::from_bytes(sealed_bytes)?, context),
                ));
        }

        for requests in groups.into_values() {
            let decrypt_requests = requests
                .iter()
                .map(|(_, request)| request.clone())
                .collect::<Vec<_>>();
            let plaintexts =
                moa_crypto::decrypt_batch(self.kms.as_ref(), &decrypt_requests).await?;
            for ((index, _), plaintext) in requests.into_iter().zip(plaintexts) {
                let content: SealedNodeContent = serde_json::from_slice(&plaintext)?;
                if content.version != SEALED_CONTENT_VERSION || !content.properties.is_object() {
                    return Err(Error::InvalidSealedContent(format!(
                        "sealed node content at index {index} has an unsupported format"
                    )));
                }
                let Some(sealed) = rows[index].as_mut() else {
                    return Err(Error::InvalidSealedContent(format!(
                        "decryption row slot {index} was unexpectedly empty"
                    )));
                };
                let row = &mut sealed.row;
                row.name = content.name;
                row.properties_summary = Some(content.properties);
            }
        }

        rows.into_iter()
            .map(|sealed| {
                sealed.map(|sealed| sealed.row).ok_or_else(|| {
                    Error::InvalidSealedContent(
                        "decryption row slot was unexpectedly empty".to_string(),
                    )
                })
            })
            .collect()
    }

    /// Opens one row's restricted/PHI `name` and `properties_summary` in place.
    ///
    /// This delegates to the same grouped boundary as multi-row reads, which
    /// validates sealed state and never accepts a plaintext restricted fallback.
    async fn decrypt_sealed_row(&self, sealed: SealedNodeRow) -> Result<NodeIndexRow, Error> {
        self.decrypt_sealed_rows(vec![sealed])
            .await?
            .pop()
            .ok_or_else(|| {
                Error::InvalidSealedContent("single-row decrypt returned no row".to_string())
            })
    }
}

#[async_trait::async_trait]
impl GraphStore for PostgresGraphStore {
    async fn create_node(&self, intent: NodeWriteIntent) -> Result<Uuid, Error> {
        crate::write::create_node(self, intent).await
    }

    async fn create_node_in_conn(
        &self,
        conn: &mut sqlx::PgConnection,
        intent: NodeWriteIntent,
    ) -> Result<Uuid, Error> {
        crate::write::create_node_in_conn(self, conn, intent).await
    }

    async fn supersede_node(&self, old_uid: Uuid, intent: NodeWriteIntent) -> Result<Uuid, Error> {
        crate::write::supersede_node(self, old_uid, intent).await
    }

    async fn reinforce_node(&self, intent: NodeReinforcementIntent) -> Result<bool, Error> {
        crate::write::reinforce_node(self, intent).await
    }

    async fn invalidate_node(&self, uid: Uuid, reason: &str) -> Result<(), Error> {
        crate::write::invalidate_node(self, uid, reason).await
    }

    async fn bulk_invalidate_nodes(&self, uids: &[Uuid], reason: &str) -> Result<Vec<Uuid>, Error> {
        crate::write::bulk_invalidate_nodes(self, uids, reason).await
    }

    async fn hard_purge(&self, uid: Uuid, redaction_marker: &str) -> Result<(), Error> {
        crate::write::hard_purge(self, uid, redaction_marker).await
    }

    async fn create_edge(&self, intent: EdgeWriteIntent) -> Result<Uuid, Error> {
        crate::write::create_edge(self, intent).await
    }

    async fn bulk_create_edges(&self, intents: Vec<EdgeWriteIntent>) -> Result<Vec<Uuid>, Error> {
        crate::write::bulk_create_edges(self, intents).await
    }

    async fn get_node(&self, uid: Uuid) -> Result<Option<NodeIndexRow>, Error> {
        let sealed = if let Some(mut conn) = self.begin().await? {
            let row = fetch_node(conn.as_mut(), uid).await?;
            conn.commit().await?;
            row
        } else {
            fetch_node(&self.pool, uid).await?
        };
        match sealed {
            Some(sealed) => Ok(Some(self.decrypt_sealed_row(sealed).await?)),
            None => Ok(None),
        }
    }

    async fn bulk_get_nodes(&self, uids: &[Uuid]) -> Result<Vec<NodeIndexRow>, Error> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let sealed = if let Some(mut conn) = self.begin().await? {
            let rows = fetch_nodes(conn.as_mut(), uids).await?;
            conn.commit().await?;
            rows
        } else {
            fetch_nodes(&self.pool, uids).await?
        };
        self.decrypt_sealed_rows(sealed).await
    }

    async fn bulk_create_nodes(&self, intents: Vec<NodeWriteIntent>) -> Result<Vec<Uuid>, Error> {
        crate::write::bulk_create_nodes(self, intents).await
    }

    async fn neighbors(
        &self,
        seed: Uuid,
        hops: u8,
        edge_filter: Option<&[EdgeLabel]>,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>, Error> {
        let max_hops = hops.clamp(1, 3);
        let limit = match max_hops {
            1 => 50_i64,
            2 => 100_i64,
            _ => 200_i64,
        };
        let edge_labels = edge_filter.map(edge_label_strings);

        let sealed = if let Some(mut conn) = self.begin().await? {
            let rows = build_neighbors_query(seed, max_hops, edge_labels.as_deref(), as_of, limit)
                .build_query_as::<SealedNodeRow>()
                .fetch_all(conn.as_mut())
                .await
                .map_err(Error::from)?;
            conn.commit().await?;
            rows
        } else {
            build_neighbors_query(seed, max_hops, edge_labels.as_deref(), as_of, limit)
                .build_query_as::<SealedNodeRow>()
                .fetch_all(&self.pool)
                .await
                .map_err(Error::from)?
        };
        self.decrypt_sealed_rows(sealed).await
    }

    async fn expand_seeds(
        &self,
        seeds: &[Uuid],
        max_hops: u8,
        as_of: Option<DateTime<Utc>>,
        scoring: &GraphWalkScoring,
        source_acl: &moa_core::types::memory::SourceAclContext,
    ) -> Result<Vec<GraphExpansionHit>, Error> {
        let seeds = unique_uids(seeds);
        if seeds.is_empty() || max_hops == 0 {
            return Ok(Vec::new());
        }

        let max_hops = max_hops.min(3);
        let limit = (seeds.len() as i64 * 200).clamp(1, 5_000);
        let raw_hits = if let Some(mut conn) = self.begin().await? {
            let rows = build_expansion_query(&seeds, max_hops, as_of, limit, scoring, source_acl)
                .build()
                .fetch_all(conn.as_mut())
                .await
                .map_err(Error::from)?;
            conn.commit().await?;
            expansion_hits_from_rows(rows)?
        } else {
            let rows = build_expansion_query(&seeds, max_hops, as_of, limit, scoring, source_acl)
                .build()
                .fetch_all(&self.pool)
                .await
                .map_err(Error::from)?;
            expansion_hits_from_rows(rows)?
        };
        if raw_hits.is_empty() {
            return Ok(Vec::new());
        }

        // Every discovered path is returned: retrieval policies gate on path
        // shape, so collapsing to one path per (seed, candidate) here would let
        // an ill-shaped path shadow an equally scored admissible one. Callers
        // that need one entry per candidate dedup after their own filtering.
        let mut hits = raw_hits
            .into_iter()
            .map(|hit| GraphExpansionHit {
                uid: hit.uid,
                label: hit.label,
                seed: hit.seed,
                seed_valid_from: hit.seed_valid_from,
                valid_from: hit.valid_from,
                hop: hit.hop,
                path_score: hit.path_score,
                edges: hit.edges,
                directions: hit.directions,
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            left.seed
                .cmp(&right.seed)
                .then_with(|| left.uid.cmp(&right.uid))
                .then_with(|| right.path_score.total_cmp(&left.path_score))
                .then_with(|| left.hop.cmp(&right.hop))
        });
        Ok(hits)
    }

    async fn lookup_seeds(
        &self,
        name: &str,
        limit: i64,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>, Error> {
        let sealed = if let Some(mut conn) = self.begin().await? {
            let rows =
                crate::node::lookup_seed_candidates(conn.as_mut(), name, limit, as_of).await?;
            conn.commit().await?;
            rows
        } else {
            crate::node::lookup_seed_candidates(&self.pool, name, limit, as_of).await?
        };
        self.decrypt_sealed_rows(sealed).await
    }

    async fn lookup_seeds_batch(
        &self,
        names: &[&str],
        limit: i64,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<Vec<NodeIndexRow>>, Error> {
        if names.is_empty() || limit <= 0 {
            return Ok(names.iter().map(|_| Vec::new()).collect());
        }
        let sealed = if let Some(mut conn) = self.begin().await? {
            let rows =
                crate::node::lookup_seed_candidate_batches(conn.as_mut(), names, limit, as_of)
                    .await?;
            conn.commit().await?;
            rows
        } else {
            crate::node::lookup_seed_candidate_batches(&self.pool, names, limit, as_of).await?
        };

        // Decrypt per name-bucket so the returned shape (one Vec per input name)
        // is preserved while restricted content is opened through the shared
        // boundary.
        let mut results = Vec::with_capacity(sealed.len());
        for bucket in sealed {
            results.push(self.decrypt_sealed_rows(bucket).await?);
        }
        Ok(results)
    }
}

fn unique_uids(uids: &[Uuid]) -> Vec<Uuid> {
    let mut seen = HashSet::new();
    let mut unique = Vec::with_capacity(uids.len());
    for uid in uids {
        if seen.insert(*uid) {
            unique.push(*uid);
        }
    }
    unique
}

fn edge_label_strings(labels: &[EdgeLabel]) -> Vec<String> {
    labels
        .iter()
        .map(|label| label.as_str().to_string())
        .collect()
}

#[derive(Debug)]
struct RawExpansionHit {
    seed: Uuid,
    uid: Uuid,
    label: NodeLabel,
    seed_valid_from: DateTime<Utc>,
    valid_from: DateTime<Utc>,
    hop: u8,
    path_score: f64,
    edges: Vec<EdgeLabel>,
    directions: Vec<GraphTraversalDirection>,
}

fn build_neighbors_query<'a>(
    seed: Uuid,
    max_hops: u8,
    edge_filter: Option<&'a [String]>,
    as_of: Option<DateTime<Utc>>,
    limit: i64,
) -> QueryBuilder<'a, Postgres> {
    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        WITH RECURSIVE walk(uid, hop, path_uids) AS (
            SELECT seed_node.uid,
                   0::int,
                   ARRAY[seed_node.uid]
            FROM moa.node_index AS seed_node
            WHERE seed_node.uid = "#,
    );
    builder.push_bind(seed);
    builder.push(" AND ");
    crate::push_validity_filter(&mut builder, Some("seed_node"), as_of);
    builder.push(
        r#"
            UNION ALL
            SELECT next_node.uid,
                   walk.hop + 1,
                   array_append(walk.path_uids, next_node.uid)
            FROM walk
            JOIN moa.edge_index AS edge_row
              ON edge_row.start_uid = walk.uid
              OR edge_row.end_uid = walk.uid
            JOIN moa.node_index AS next_node
              ON next_node.uid = CASE
                  WHEN edge_row.start_uid = walk.uid THEN edge_row.end_uid
                  ELSE edge_row.start_uid
              END
            WHERE walk.hop < "#,
    );
    builder.push_bind(i32::from(max_hops));
    builder.push(" AND ");
    crate::push_validity_filter(&mut builder, Some("edge_row"), as_of);
    builder.push(" AND ");
    crate::push_validity_filter(&mut builder, Some("next_node"), as_of);
    builder.push(" AND NOT next_node.uid = ANY(walk.path_uids)");
    if let Some(labels) = edge_filter.filter(|labels| !labels.is_empty()) {
        builder.push(" AND edge_row.label = ANY(");
        builder.push_bind(labels);
        builder.push(")");
    }
    builder.push(
        r#"
        ),
        reached AS (
            SELECT uid, MIN(hop) AS hop
            FROM walk
            WHERE hop > 0
            GROUP BY uid
        )
        SELECT node_row.uid, node_row.label, node_row.storage_partition_id, node_row.user_id,
               node_row.scope, node_row.name, node_row.pii_class, node_row.valid_to,
               node_row.valid_from, node_row.properties_summary, node_row.last_accessed_at,
               COALESCE(node_row.quality_score, 0.5) AS quality_score,
               node_row.content_sealed, node_row.tenant_id, node_row.data_subject_id
        FROM reached
        JOIN moa.node_index AS node_row ON node_row.uid = reached.uid
        WHERE "#,
    );
    crate::push_validity_filter(&mut builder, Some("node_row"), as_of);
    builder.push(
        r#"
        ORDER BY reached.hop, node_row.uid
        LIMIT "#,
    );
    builder.push_bind(limit);
    builder
}

/// Appends a `CASE step-prior END` expression over the scoring's edge priors.
fn push_edge_prior_case(builder: &mut QueryBuilder<'_, Postgres>, scoring: &GraphWalkScoring) {
    if scoring.edge_priors.is_empty() {
        builder.push("1.0::float8");
        return;
    }
    builder.push("CASE edge_row.label");
    for (label, prior) in &scoring.edge_priors {
        // Labels come from the closed `EdgeLabel` enum, so inlining the SQL
        // string literal is safe; the prior value is bound.
        builder.push(format!(" WHEN '{}' THEN ", label.as_str()));
        builder.push_bind(*prior);
    }
    builder.push(" ELSE 1.0::float8 END");
}

fn build_expansion_query<'a>(
    seeds: &'a [Uuid],
    max_hops: u8,
    as_of: Option<DateTime<Utc>>,
    limit: i64,
    scoring: &'a GraphWalkScoring,
    source_acl: &'a moa_core::types::memory::SourceAclContext,
) -> QueryBuilder<'a, Postgres> {
    // Seed-anchored traversal: rather than materializing every visible node in
    // the tenant into a CTE and joining against it (which forced a full
    // `node_index` scan before the `LIMIT`), the base case is a primary-key
    // lookup on the seed uids and each recursive hop joins `edge_index` and
    // `node_index` directly with the validity filter inlined into the JOIN. This
    // lets Postgres drive the walk from the seeds outward using the edge and uid
    // indexes.
    //
    // Each hop follows edges in both directions. Instead of a single
    // `edge_row.start_uid = walk.uid OR edge_row.end_uid = walk.uid` join — which
    // Postgres cannot service with either single-column edge index — the step is
    // a `LATERAL` of two index-friendly branches: one keyed on `start_uid` (uses
    // `edge_index(tenant_id, start_uid)`), one on `end_uid` (uses
    // `edge_index(tenant_id, end_uid)`). The two forms enumerate the same
    // (neighbor, edge label) multiset for every walk row: a non-self-loop edge
    // matches exactly one branch (the endpoint equal to `walk.uid`), and a
    // self-loop matches both branches but is dropped by the path-cycle guard just
    // as it was under the `OR` form.
    //
    // Each branch also emits the edge-label prior, so the recursive arm can
    // accumulate `path_score = decay^hop * product(priors)` and prune branches
    // below the configured threshold inside the walk. The final ordering keeps
    // the best-scoring paths when the row limit truncates hub fan-out.
    let mut builder =
        QueryBuilder::<Postgres>::new("WITH RECURSIVE seed_uids(uid) AS (SELECT unnest(");
    builder.push_bind(seeds);
    builder.push(
        r#"::uuid[])),
        walk(seed, uid, label, seed_valid_from, valid_from, hop, path_score, edges, directions, path_uids) AS (
            SELECT seed_uids.uid,
                   seed_node.uid,
                   seed_node.label,
                   seed_node.valid_from,
                   seed_node.valid_from,
                   0::int,
                   1.0::float8,
                   ARRAY[]::text[],
                   ARRAY[]::text[],
                   ARRAY[seed_node.uid]
            FROM seed_uids
            JOIN moa.node_index AS seed_node
              ON seed_node.uid = seed_uids.uid
             AND "#,
    );
    crate::push_validity_filter(&mut builder, Some("seed_node"), as_of);
    // A denied seed must not enter the walk at all: admitting it "just as a
    // starting point" would let the caller reach its neighbours, which is the
    // disclosure the seed itself was refused for.
    builder.push(" AND ");
    moa_db::push_source_acl_predicate(&mut builder, "seed_node.uid", source_acl);
    builder.push(
        r#"
            UNION ALL
            SELECT walk.seed,
                   next_node.uid,
                   next_node.label,
                   walk.seed_valid_from,
                   next_node.valid_from,
                   walk.hop + 1,
                   walk.path_score * "#,
    );
    builder.push_bind(scoring.decay);
    builder.push(
        r#" * step.prior,
                   array_append(walk.edges, step.label),
                   array_append(walk.directions, step.direction),
                   array_append(walk.path_uids, next_node.uid)
            FROM walk
            JOIN LATERAL (
                SELECT edge_row.end_uid AS neighbor_uid,
                       edge_row.label AS label,
                       'outgoing'::text AS direction,
                       "#,
    );
    push_edge_prior_case(&mut builder, scoring);
    builder.push(
        r#" AS prior
                FROM moa.edge_index AS edge_row
                WHERE edge_row.start_uid = walk.uid
                  AND "#,
    );
    crate::push_validity_filter(&mut builder, Some("edge_row"), as_of);
    builder.push(
        r#"
                UNION ALL
                SELECT edge_row.start_uid AS neighbor_uid,
                       edge_row.label AS label,
                       'incoming'::text AS direction,
                       "#,
    );
    push_edge_prior_case(&mut builder, scoring);
    builder.push(
        r#" AS prior
                FROM moa.edge_index AS edge_row
                WHERE edge_row.end_uid = walk.uid
                  AND "#,
    );
    crate::push_validity_filter(&mut builder, Some("edge_row"), as_of);
    builder.push(
        r#"
            ) AS step ON true
            JOIN moa.node_index AS next_node
              ON next_node.uid = step.neighbor_uid
             AND "#,
    );
    crate::push_validity_filter(&mut builder, Some("next_node"), as_of);
    // Applied inside the recursive JOIN rather than to the final SELECT, so a
    // denied node is removed from the frontier and cannot bridge the walk to a
    // node the caller would otherwise never have reached.
    builder.push(" AND ");
    moa_db::push_source_acl_predicate(&mut builder, "next_node.uid", source_acl);
    builder.push(" WHERE walk.hop < ");
    builder.push_bind(i32::from(max_hops));
    builder.push(" AND walk.path_score * ");
    builder.push_bind(scoring.decay);
    builder.push(" * step.prior >= ");
    builder.push_bind(scoring.prune_below);
    builder.push(
        r#"
              AND NOT next_node.uid = ANY(walk.path_uids)
        )
        SELECT seed, uid, label, seed_valid_from, valid_from, hop, path_score, edges, directions
        FROM walk
        WHERE hop > 0
        ORDER BY path_score DESC, seed, uid, hop, edges
        LIMIT "#,
    );
    builder.push_bind(limit);
    builder
}

fn expansion_hits_from_rows(
    rows: Vec<sqlx::postgres::PgRow>,
) -> Result<Vec<RawExpansionHit>, Error> {
    rows.into_iter()
        .map(|row| {
            let seed: Uuid = row.try_get("seed")?;
            let uid: Uuid = row.try_get("uid")?;
            let label_text: String = row.try_get("label")?;
            let label = label_text.parse()?;
            let seed_valid_from: DateTime<Utc> = row.try_get("seed_valid_from")?;
            let valid_from: DateTime<Utc> = row.try_get("valid_from")?;
            let hop_i32: i32 = row.try_get("hop")?;
            let hop = u8::try_from(hop_i32).map_err(|error| {
                Error::GraphQuery(format!(
                    "expansion returned invalid hop `{hop_i32}`: {error}"
                ))
            })?;
            let path_score: f64 = row.try_get("path_score")?;
            let edge_texts: Vec<String> = row.try_get("edges")?;
            let edges = edge_texts
                .into_iter()
                .map(|label| label.parse())
                .collect::<Result<Vec<_>, _>>()?;
            let direction_texts: Vec<String> = row.try_get("directions")?;
            let directions = direction_texts
                .into_iter()
                .map(|direction| direction.parse())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(RawExpansionHit {
                seed,
                uid,
                label,
                seed_valid_from,
                valid_from,
                hop,
                path_score,
                edges,
                directions,
            })
        })
        .collect()
}

async fn fetch_node<'e, E>(executor: E, uid: Uuid) -> Result<Option<SealedNodeRow>, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SealedNodeRow>(&format!(
        "SELECT {NODE_INDEX_COLUMNS}, {SEALED_NODE_INDEX_EXTRA_COLUMNS} \
         FROM moa.node_index WHERE uid = $1"
    ))
    .bind(uid)
    .fetch_optional(executor)
    .await
    .map_err(Error::from)
}

async fn fetch_nodes<'e, E>(executor: E, uids: &[Uuid]) -> Result<Vec<SealedNodeRow>, Error>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SealedNodeRow>(&format!(
        "SELECT {NODE_INDEX_COLUMNS}, {SEALED_NODE_INDEX_EXTRA_COLUMNS} \
         FROM moa.node_index WHERE uid = ANY($1)"
    ))
    .bind(uids)
    .fetch_all(executor)
    .await
    .map_err(Error::from)
}
