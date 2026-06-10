//! Read-side implementation for the AGE graph store.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use sqlx::{Postgres, QueryBuilder, Row};

use crate::{
    GraphError, GraphExpansionHit, GraphStore,
    age::AgeGraphStore,
    cypher,
    edge::{EdgeLabel, EdgeWriteIntent},
    lexical,
    node::{NodeIndexRow, NodeLabel, NodeWriteIntent},
};

const NODE_LABELS: [NodeLabel; 7] = [
    NodeLabel::Entity,
    NodeLabel::Concept,
    NodeLabel::Decision,
    NodeLabel::Incident,
    NodeLabel::Lesson,
    NodeLabel::Fact,
    NodeLabel::Source,
];

const EDGE_LABELS: [EdgeLabel; 9] = [
    EdgeLabel::RelatesTo,
    EdgeLabel::DependsOn,
    EdgeLabel::Supersedes,
    EdgeLabel::Contradicts,
    EdgeLabel::DerivedFrom,
    EdgeLabel::MentionedIn,
    EdgeLabel::Caused,
    EdgeLabel::LearnedFrom,
    EdgeLabel::AppliesTo,
];

#[async_trait::async_trait]
impl GraphStore for AgeGraphStore {
    async fn create_node(&self, intent: NodeWriteIntent) -> Result<Uuid, GraphError> {
        crate::write::create_node(self, intent).await
    }

    async fn create_node_in_conn(
        &self,
        conn: &mut sqlx::PgConnection,
        intent: NodeWriteIntent,
    ) -> Result<Uuid, GraphError> {
        crate::write::create_node_in_conn(self, conn, intent).await
    }

    async fn supersede_node(
        &self,
        old_uid: Uuid,
        intent: NodeWriteIntent,
    ) -> Result<Uuid, GraphError> {
        crate::write::supersede_node(self, old_uid, intent).await
    }

    async fn invalidate_node(&self, uid: Uuid, reason: &str) -> Result<(), GraphError> {
        crate::write::invalidate_node(self, uid, reason).await
    }

    async fn hard_purge(&self, uid: Uuid, redaction_marker: &str) -> Result<(), GraphError> {
        crate::write::hard_purge(self, uid, redaction_marker).await
    }

    async fn create_edge(&self, intent: EdgeWriteIntent) -> Result<Uuid, GraphError> {
        crate::write::create_edge(self, intent).await
    }

    async fn get_node(&self, uid: Uuid) -> Result<Option<NodeIndexRow>, GraphError> {
        if let Some(mut conn) = self.begin().await? {
            let row = fetch_node(conn.as_mut(), uid).await?;
            conn.commit().await?;
            return Ok(row);
        }

        sqlx::query_as::<_, NodeIndexRow>(
            r#"
            SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
                   valid_to, valid_from, properties_summary, last_accessed_at
            FROM moa.node_index
            WHERE uid = $1
            "#,
        )
        .bind(uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(GraphError::from)
    }

    async fn neighbors(
        &self,
        seed: Uuid,
        hops: u8,
        edge_filter: Option<&[EdgeLabel]>,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        if edge_filter.is_some_and(|labels| !labels.is_empty()) {
            return Err(GraphError::Conflict(
                "edge-filtered neighbors require a dedicated traversal template".to_string(),
            ));
        }

        let (template, limit) = match hops {
            0 | 1 if as_of.is_some() => (&cypher::traverse::NEIGHBORS_1HOP_AS_OF, 50_i64),
            0 | 1 => (&cypher::traverse::NEIGHBORS_1HOP, 50_i64),
            2 if as_of.is_some() => (&cypher::traverse::NEIGHBORS_2HOP_AS_OF, 100_i64),
            2 => (&cypher::traverse::NEIGHBORS_2HOP, 100_i64),
            _ if as_of.is_some() => (&cypher::traverse::NEIGHBORS_3HOP_AS_OF, 200_i64),
            _ => (&cypher::traverse::NEIGHBORS_3HOP, 200_i64),
        };
        let mut params = serde_json::json!({
            "seed_uid": seed.to_string(),
            "limit": limit,
        });
        if let Some(as_of) = as_of
            && let Some(object) = params.as_object_mut()
        {
            object.insert("as_of".to_string(), serde_json::json!(as_of.to_rfc3339()));
        }

        let uid_texts = if let Some(mut conn) = self.begin().await? {
            let rows = template
                .execute(&params)
                .fetch_all(conn.as_mut())
                .await
                .map_err(GraphError::from)?;
            conn.commit().await?;
            rows.into_iter()
                .map(|row| row.try_get::<String, _>(0))
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let rows = template
                .execute(&params)
                .fetch_all(&self.pool)
                .await
                .map_err(GraphError::from)?;
            rows.into_iter()
                .map(|row| row.try_get::<String, _>(0))
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        let uids = uid_texts
            .iter()
            .filter_map(|value| parse_agtype_uuid(value))
            .collect::<Vec<_>>();
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        fetch_nodes_by_uid(self, &uids, as_of).await
    }

    async fn expand_seeds(
        &self,
        seeds: &[Uuid],
        max_hops: u8,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<GraphExpansionHit>, GraphError> {
        let seeds = unique_uids(seeds);
        if seeds.is_empty() || max_hops == 0 {
            return Ok(Vec::new());
        }

        let max_hops = max_hops.min(3);
        let limit = (seeds.len() as i64 * 200).clamp(1, 5_000);
        let raw_hits = if let Some(mut conn) = self.begin().await? {
            let rows = build_expansion_query(&seeds, max_hops, as_of, limit)
                .build()
                .fetch_all(conn.as_mut())
                .await
                .map_err(GraphError::from)?;
            conn.commit().await?;
            expansion_hits_from_rows(rows)?
        } else {
            let rows = build_expansion_query(&seeds, max_hops, as_of, limit)
                .build()
                .fetch_all(&self.pool)
                .await
                .map_err(GraphError::from)?;
            expansion_hits_from_rows(rows)?
        };
        if raw_hits.is_empty() {
            return Ok(Vec::new());
        }

        let mut shortest_hits = HashMap::<(Uuid, Uuid), RawExpansionHit>::new();
        for hit in raw_hits {
            let key = (hit.seed, hit.uid);
            let replace = shortest_hits
                .get(&key)
                .is_none_or(|stored| hit.hop < stored.hop);
            if replace {
                shortest_hits.insert(key, hit);
            }
        }

        let mut hits = shortest_hits
            .into_values()
            .map(|hit| GraphExpansionHit {
                uid: hit.uid,
                label: hit.label,
                seed: hit.seed,
                seed_valid_from: hit.seed_valid_from,
                valid_from: hit.valid_from,
                hop: hit.hop,
                edges: hit.edges,
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            left.seed
                .cmp(&right.seed)
                .then_with(|| left.uid.cmp(&right.uid))
        });
        Ok(hits)
    }

    async fn lookup_seeds(
        &self,
        name: &str,
        limit: i64,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        if let Some(mut conn) = self.begin().await? {
            let rows = crate::node::lookup_seed_by_name(conn.as_mut(), name, limit, as_of).await?;
            conn.commit().await?;
            return Ok(rows);
        }

        lexical::lookup_seed_rows(&self.pool, name, limit, as_of).await
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

#[derive(Debug)]
struct RawExpansionHit {
    seed: Uuid,
    uid: Uuid,
    label: NodeLabel,
    seed_valid_from: DateTime<Utc>,
    valid_from: DateTime<Utc>,
    hop: u8,
    edges: Vec<EdgeLabel>,
}

fn build_expansion_query<'a>(
    seeds: &'a [Uuid],
    max_hops: u8,
    as_of: Option<DateTime<Utc>>,
    limit: i64,
) -> QueryBuilder<'a, Postgres> {
    let mut builder =
        QueryBuilder::<Postgres>::new("WITH RECURSIVE seed_uids(uid) AS (SELECT unnest(");
    builder.push_bind(seeds);
    builder.push(
        r#"::uuid[])),
        visible_nodes AS (
            SELECT vertex.id, node.uid, node.label, node.valid_from
            FROM ("#,
    );
    push_vertex_union(&mut builder);
    builder.push(
        r#"
            ) AS vertex
            JOIN moa.node_index AS node ON node.uid = vertex.uid
            WHERE "#,
    );
    crate::push_validity_filter(&mut builder, Some("node"), as_of);
    builder.push(
        r#"
        ),
        edge_rows AS ("#,
    );
    push_edge_union(&mut builder);
    builder.push(
        r#"
        ),
        walk(seed, uid, label, seed_valid_from, valid_from, vertex_id, hop, edges, path_ids) AS (
            SELECT seed_uids.uid,
                   visible_nodes.uid,
                   visible_nodes.label,
                   visible_nodes.valid_from,
                   visible_nodes.valid_from,
                   visible_nodes.id,
                   0::int,
                   ARRAY[]::text[],
                   ARRAY[visible_nodes.id]
            FROM seed_uids
            JOIN visible_nodes ON visible_nodes.uid = seed_uids.uid
            UNION ALL
            SELECT walk.seed,
                   next_node.uid,
                   next_node.label,
                   walk.seed_valid_from,
                   next_node.valid_from,
                   next_node.id,
                   walk.hop + 1,
                   walk.edges || edge_rows.label,
                   walk.path_ids || next_node.id
            FROM walk
            JOIN edge_rows
              ON edge_rows.start_id = walk.vertex_id
            JOIN visible_nodes AS next_node
              ON next_node.id = edge_rows.end_id
            WHERE walk.hop < "#,
    );
    builder.push_bind(i32::from(max_hops));
    builder.push(
        r#"
              AND NOT next_node.id = ANY(walk.path_ids)
        )
        SELECT seed, uid, label, seed_valid_from, valid_from, hop, edges
        FROM walk
        WHERE hop > 0
        ORDER BY seed, uid, hop, edges
        LIMIT "#,
    );
    builder.push_bind(limit);
    builder
}

fn push_vertex_union(builder: &mut QueryBuilder<'_, Postgres>) {
    for (index, label) in NODE_LABELS.iter().enumerate() {
        if index > 0 {
            builder.push(" UNION ALL ");
        }
        builder.push("SELECT id, trim(both '\"' from moa.age_property(properties, 'uid')::text)::uuid AS uid FROM ");
        push_age_table(builder, label.as_str());
    }
}

fn push_edge_union(builder: &mut QueryBuilder<'_, Postgres>) {
    for (index, label) in EDGE_LABELS.iter().enumerate() {
        if index > 0 {
            builder.push(" UNION ALL ");
        }
        builder.push("SELECT start_id, end_id, ");
        builder.push_bind(label.as_str());
        builder.push("::text AS label FROM ");
        push_age_table(builder, label.as_str());
    }
}

fn push_age_table(builder: &mut QueryBuilder<'_, Postgres>, label: &str) {
    builder.push("moa_graph.\"");
    builder.push(label);
    builder.push("\"");
}

fn expansion_hits_from_rows(
    rows: Vec<sqlx::postgres::PgRow>,
) -> Result<Vec<RawExpansionHit>, GraphError> {
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
                GraphError::Cypher(format!(
                    "expansion returned invalid hop `{hop_i32}`: {error}"
                ))
            })?;
            let edge_texts: Vec<String> = row.try_get("edges")?;
            let edges = edge_texts
                .into_iter()
                .map(|label| label.parse())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(RawExpansionHit {
                seed,
                uid,
                label,
                seed_valid_from,
                valid_from,
                hop,
                edges,
            })
        })
        .collect()
}

async fn fetch_node(
    conn: &mut sqlx::PgConnection,
    uid: Uuid,
) -> Result<Option<NodeIndexRow>, GraphError> {
    sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE uid = $1
        "#,
    )
    .bind(uid)
    .fetch_optional(conn)
    .await
    .map_err(GraphError::from)
}

async fn fetch_nodes_by_uid(
    store: &AgeGraphStore,
    uids: &[Uuid],
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<NodeIndexRow>, GraphError> {
    if let Some(mut conn) = store.begin().await? {
        let rows = fetch_nodes(conn.as_mut(), uids, as_of).await?;
        conn.commit().await?;
        return Ok(rows);
    }

    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE uid = ANY(
        "#,
    );
    builder.push_bind(uids);
    builder.push(") AND ");
    crate::push_validity_filter(&mut builder, None, as_of);
    builder
        .build_query_as::<NodeIndexRow>()
        .fetch_all(&store.pool)
        .await
        .map_err(GraphError::from)
}

async fn fetch_nodes(
    conn: &mut sqlx::PgConnection,
    uids: &[Uuid],
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<NodeIndexRow>, GraphError> {
    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE uid = ANY(
        "#,
    );
    builder.push_bind(uids);
    builder.push(") AND ");
    crate::push_validity_filter(&mut builder, None, as_of);
    builder
        .build_query_as::<NodeIndexRow>()
        .fetch_all(conn)
        .await
        .map_err(GraphError::from)
}

fn parse_agtype_uuid(value: &str) -> Option<Uuid> {
    let trimmed = value.trim().trim_matches('"');
    Uuid::parse_str(trimmed).ok()
}
