//! Typed Cypher templates.
//!
//! User input never gets interpolated into Cypher. Each template is a static SQL wrapper around a
//! fixed Cypher body and receives one agtype parameter map at execution time.

use serde_json::Value;
use sqlx::{
    Postgres, Type,
    encode::{Encode, IsNull},
    error::BoxDynError,
    postgres::{PgArgumentBuffer, PgArguments, PgTypeInfo},
    query::Query,
};

/// One static Cypher template and its prepared SQL wrapper.
pub struct Cypher {
    sql: &'static str,
}

impl Cypher {
    /// Creates a static Cypher template wrapper.
    pub const fn new(sql: &'static str) -> Self {
        Self { sql }
    }

    /// Builds a sqlx query for this template with a single agtype parameter map.
    pub fn execute(&self, params: &Value) -> Query<'_, Postgres, PgArguments> {
        sqlx::query(self.sql).bind(AgTypeParam(params.to_string()))
    }
}

macro_rules! cypher_sql {
    ($($body:tt)*) => {
        concat!(
            "SELECT result::text FROM ag_catalog.cypher('moa_graph', $$ ",
            $($body)*,
            " $$, $1) AS (result ag_catalog.agtype)"
        )
    };
}

#[derive(Debug, Clone)]
struct AgTypeParam(String);

impl Type<Postgres> for AgTypeParam {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_name("agtype")
    }
}

impl Encode<'_, Postgres> for AgTypeParam {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        // AGE's binary receive path follows the jsonb convention: a one-byte version prefix
        // followed by the textual agtype representation.
        buf.extend(&[1]);
        buf.extend(self.0.as_bytes());
        Ok(IsNull::No)
    }
}

pub mod node {
    //! Node mutation Cypher templates.

    use super::Cypher;

    macro_rules! create_node_template {
        ($name:ident, $label:literal, $doc:literal) => {
            #[doc = $doc]
            pub const $name: Cypher = Cypher::new(cypher_sql!(
                "CREATE (n:",
                $label,
                " {uid: $uid, workspace_id: $workspace_id, user_id: $user_id, ",
                "scope: $scope, name: $name, pii_class: $pii_class}) RETURN n.uid AS result"
            ));
        };
    }

    create_node_template!(CREATE_ENTITY, "Entity", "Create an `Entity` node.");
    create_node_template!(CREATE_CONCEPT, "Concept", "Create a `Concept` node.");
    create_node_template!(CREATE_DECISION, "Decision", "Create a `Decision` node.");
    create_node_template!(CREATE_INCIDENT, "Incident", "Create an `Incident` node.");
    create_node_template!(CREATE_LESSON, "Lesson", "Create a `Lesson` node.");
    create_node_template!(CREATE_FACT, "Fact", "Create a `Fact` node.");
    create_node_template!(CREATE_SOURCE, "Source", "Create a `Source` node.");

    /// Fetch an `Entity` node uid for AGE smoke coverage.
    pub const GET_ENTITY_UID: Cypher = Cypher::new(cypher_sql!(
        "MATCH (n:Entity {uid: $uid}) RETURN n.uid AS result"
    ));

    /// Supersede a node by invalidating the old node and creating a new node.
    pub const SUPERSEDE: Cypher = Cypher::new(cypher_sql!(
        "MATCH (old {uid: $old_uid}) \
         SET old.valid_to = $now, old.invalidated_at = $now, \
             old.invalidated_by = $actor, old.invalidated_reason = 'superseded' \
         WITH old \
         CREATE (new {uid: $uid, workspace_id: $workspace_id, user_id: $user_id, \
                      scope: $scope, name: $name, pii_class: $pii_class}) \
         CREATE (new)-[:SUPERSEDES {uid: $edge_uid, workspace_id: $workspace_id, \
                                    user_id: $user_id, scope: $scope}]->(old) \
         RETURN new.uid AS result"
    ));

    /// Soft-invalidate a node.
    pub const INVALIDATE: Cypher = Cypher::new(cypher_sql!(
        "MATCH (n {uid: $uid}) \
         SET n.valid_to = $now, n.invalidated_at = $now, \
             n.invalidated_by = $actor, n.invalidated_reason = $reason \
         RETURN n.uid AS result"
    ));

    /// Detach-delete a node from AGE.
    pub const HARD_PURGE: Cypher = Cypher::new(cypher_sql!(
        "MATCH (n {uid: $uid}) DETACH DELETE n RETURN $uid"
    ));
}

pub mod edge {
    //! Edge mutation Cypher templates.

    use super::Cypher;

    macro_rules! create_edge_template {
        ($name:ident, $label:literal, $doc:literal) => {
            #[doc = $doc]
            pub const $name: Cypher = Cypher::new(cypher_sql!(
                "MATCH (a {uid: $start_uid}), (b {uid: $end_uid}) ",
                "CREATE (a)-[r:",
                $label,
                " {uid: $uid, workspace_id: $workspace_id, user_id: $user_id, ",
                "scope: $scope}]->(b) RETURN r.uid AS result"
            ));
        };
    }

    create_edge_template!(
        CREATE_RELATES_TO,
        "RELATES_TO",
        "Create a `RELATES_TO` edge."
    );
    create_edge_template!(
        CREATE_DEPENDS_ON,
        "DEPENDS_ON",
        "Create a `DEPENDS_ON` edge."
    );
    create_edge_template!(
        CREATE_SUPERSEDES,
        "SUPERSEDES",
        "Create a `SUPERSEDES` edge."
    );
    create_edge_template!(
        CREATE_CONTRADICTS,
        "CONTRADICTS",
        "Create a `CONTRADICTS` edge."
    );
    create_edge_template!(
        CREATE_DERIVED_FROM,
        "DERIVED_FROM",
        "Create a `DERIVED_FROM` edge."
    );
    create_edge_template!(
        CREATE_MENTIONED_IN,
        "MENTIONED_IN",
        "Create a `MENTIONED_IN` edge."
    );
    create_edge_template!(CREATE_CAUSED, "CAUSED", "Create a `CAUSED` edge.");
    create_edge_template!(
        CREATE_LEARNED_FROM,
        "LEARNED_FROM",
        "Create a `LEARNED_FROM` edge."
    );
    create_edge_template!(
        CREATE_APPLIES_TO,
        "APPLIES_TO",
        "Create an `APPLIES_TO` edge."
    );
}

pub mod traverse {
    //! Traversal Cypher templates.

    use super::Cypher;

    /// One-hop undirected neighbor traversal.
    pub const NEIGHBORS_1HOP: Cypher = Cypher::new(cypher_sql!(
        "MATCH (s {uid: $seed_uid})-[*1..1]-(n) \
         WHERE n.valid_to IS NULL \
         RETURN DISTINCT n.uid AS result LIMIT $limit"
    ));

    /// Two-hop undirected neighbor traversal.
    pub const NEIGHBORS_2HOP: Cypher = Cypher::new(cypher_sql!(
        "MATCH (s {uid: $seed_uid})-[*1..2]-(n) \
         WHERE n.valid_to IS NULL \
         RETURN DISTINCT n.uid AS result LIMIT $limit"
    ));

    /// Three-hop undirected neighbor traversal.
    pub const NEIGHBORS_3HOP: Cypher = Cypher::new(cypher_sql!(
        "MATCH (s {uid: $seed_uid})-[*1..3]-(n) \
         WHERE n.valid_to IS NULL \
         RETURN DISTINCT n.uid AS result LIMIT $limit"
    ));
}
