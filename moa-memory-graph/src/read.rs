//! Read-side implementation for the AGE graph store.

use uuid::Uuid;

use sqlx::Row;

use crate::{
    GraphError, GraphStore,
    age::AgeGraphStore,
    cypher,
    edge::{EdgeLabel, EdgeWriteIntent},
    lexical,
    node::{NodeIndexRow, NodeWriteIntent},
};

#[async_trait::async_trait]
impl GraphStore for AgeGraphStore {
    async fn create_node(&self, intent: NodeWriteIntent) -> Result<Uuid, GraphError> {
        crate::write::create_node(self, intent).await
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
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        if edge_filter.is_some_and(|labels| !labels.is_empty()) {
            return Err(GraphError::Conflict(
                "edge-filtered neighbors require a dedicated traversal template".to_string(),
            ));
        }

        let (template, limit) = match hops {
            0 | 1 => (&cypher::traverse::NEIGHBORS_1HOP, 50_i64),
            2 => (&cypher::traverse::NEIGHBORS_2HOP, 100_i64),
            _ => (&cypher::traverse::NEIGHBORS_3HOP, 200_i64),
        };
        let params = serde_json::json!({
            "seed_uid": seed.to_string(),
            "limit": limit,
        });

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

        fetch_nodes_by_uid(self, &uids).await
    }

    async fn lookup_seeds(&self, name: &str, limit: i64) -> Result<Vec<NodeIndexRow>, GraphError> {
        if let Some(mut conn) = self.begin().await? {
            let rows = crate::node::lookup_seed_by_name(conn.as_mut(), name, limit).await?;
            conn.commit().await?;
            return Ok(rows);
        }

        lexical::lookup_seed_rows(&self.pool, name, limit).await
    }
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
) -> Result<Vec<NodeIndexRow>, GraphError> {
    if let Some(mut conn) = store.begin().await? {
        let rows = fetch_nodes(conn.as_mut(), uids).await?;
        conn.commit().await?;
        return Ok(rows);
    }

    sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE uid = ANY($1)
          AND valid_to IS NULL
        "#,
    )
    .bind(uids)
    .fetch_all(&store.pool)
    .await
    .map_err(GraphError::from)
}

async fn fetch_nodes(
    conn: &mut sqlx::PgConnection,
    uids: &[Uuid],
) -> Result<Vec<NodeIndexRow>, GraphError> {
    sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE uid = ANY($1)
          AND valid_to IS NULL
        "#,
    )
    .bind(uids)
    .fetch_all(conn)
    .await
    .map_err(GraphError::from)
}

fn parse_agtype_uuid(value: &str) -> Option<Uuid> {
    let trimmed = value.trim().trim_matches('"');
    Uuid::parse_str(trimmed).ok()
}
