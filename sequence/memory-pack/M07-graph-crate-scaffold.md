# Step M07 — `moa-memory-graph` crate scaffold (GraphStore trait + AGE adapter + Cypher templates)

_Establish the canonical Rust crate for graph operations: the `GraphStore` trait, the AGE-backed implementation, and a typed library of Cypher templates so consumers never construct Cypher strings inline._

## 1 What this step is about

This crate establishes `GraphStore`, `VectorStore` integration, and `LexicalStore` as separate responsibilities. Cypher is dangerous if string-formatted — every template in this crate uses parameterized agtype maps, never `format!("MATCH ... {}", ...)` interpolation.

## 2 Files to read

- M03 AGE bootstrap (label list)
- M04 sidecar projection (`NodeIndexRow`, `NodeLabel`)
- M06 changelog (`ChangelogRecord`, `write_and_bump`)
- Existing graph/vector crate docs under `crates/moa-memory/`

## 3 Goal

1. `moa-memory-graph` crate with: `GraphStore` trait, `AgeGraphStore` impl, `LexicalStore` over `moa.node_index.name_tsv`, Cypher template library, and bi-temporal write helpers (M08 fills these).
2. Smoke test: insert a node via the trait, fetch it back.

## 4 Rules

- **No Cypher string formatting.** Every Cypher invocation goes through `Cypher::Tmpl(...).execute(&mut conn, params)`, where `params` is a `serde_json::Value` map serialized into agtype.
- **Trait methods are async** (`#[async_trait]`).
- **Crate has no direct dep on `moa-memory`.** Forward direction only.
- **Errors flow through one type** `GraphError` with `thiserror`-derived variants for `Cypher`, `Sidecar`, `RlsDenied`, `NotFound`, `BiTemporal(...)`, `Conflict(...)`.

## 5 Tasks

### 5a Cargo.toml

```toml
[package]
name = "moa-memory-graph"
version.workspace = true
edition.workspace = true

[dependencies]
async-trait = "0.1"
sqlx = { workspace = true, features = ["postgres","uuid","chrono","macros","json"] }
uuid = { workspace = true }
chrono = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = "1"
anyhow = "1"
tracing = "0.1"
moa-core = { path = "../moa-core" }
moa-runtime = { path = "../moa-runtime" }
```

### 5b GraphStore trait (`crates/moa-memory/graph/src/lib.rs`)

```rust
use async_trait::async_trait;
use uuid::Uuid;

pub mod age;
pub mod cypher;
pub mod node;
pub mod edge;
pub mod changelog;
pub mod write;
pub mod read;
pub mod error;

pub use error::GraphError;
pub use node::{NodeLabel, NodeIndexRow, NodeWriteIntent};
pub use edge::{EdgeLabel, EdgeWriteIntent};

#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Create a new node, sidecar projection, and changelog row in one transaction.
    async fn create_node(&self, intent: NodeWriteIntent) -> Result<Uuid, GraphError>;

    /// Update properties on an existing node (create new node + supersede in M08).
    async fn supersede_node(&self, old_uid: Uuid, intent: NodeWriteIntent) -> Result<Uuid, GraphError>;

    /// Soft-invalidate (set valid_to + invalidated_*).
    async fn invalidate_node(&self, uid: Uuid, reason: &str) -> Result<(), GraphError>;

    /// Hard-purge (remove from AGE + sidecar; redaction marker in changelog). Used by M24.
    async fn hard_purge(&self, uid: Uuid, redaction_marker: &str) -> Result<(), GraphError>;

    /// Create an edge between two nodes; sidecar may not need to project edges in v1.
    async fn create_edge(&self, intent: EdgeWriteIntent) -> Result<Uuid, GraphError>;

    /// Lookup a single node by uid.
    async fn get_node(&self, uid: Uuid) -> Result<Option<NodeIndexRow>, GraphError>;

    /// Traverse: 1-3 hop neighbors of a seed, scope-bounded by current GUC.
    async fn neighbors(&self, seed: Uuid, hops: u8, edge_filter: Option<&[EdgeLabel]>) -> Result<Vec<NodeIndexRow>, GraphError>;

    /// NER seed lookup via tsvector on name.
    async fn lookup_seeds(&self, name: &str, limit: i64) -> Result<Vec<NodeIndexRow>, GraphError>;
}
```

### 5c AGE adapter struct

`crates/moa-memory/graph/src/age.rs`:

```rust
use sqlx::PgPool;
use moa_runtime::ScopeContext;

pub struct AgeGraphStore { pool: PgPool }
impl AgeGraphStore { pub fn new(pool: PgPool) -> Self { Self { pool } } }

// trait impl is split across read.rs / write.rs (M08 finishes the writes)
```

### 5d Cypher template library

`crates/moa-memory/graph/src/cypher.rs`:

```rust
//! Typed Cypher templates. Never interpolate user input into Cypher strings.
//! All parameters travel as a single agtype map bound at execute-time.

use serde_json::Value;

pub struct Cypher { pub query: &'static str }

impl Cypher {
    pub fn execute<'c, 'q>(
        &'q self, params: &Value,
    ) -> sqlx::query::Query<'c, sqlx::Postgres, sqlx::postgres::PgArguments>
    where 'q: 'c {
        sqlx::query(&format!(
            "SELECT * FROM cypher('moa_graph', $$ {} $$, $1) AS (result agtype)",
            self.query
        )).bind(params)
    }
}

pub mod node {
    use super::Cypher;
    pub const CREATE_FACT: Cypher = Cypher { query: "
        CREATE (n:Fact $props) RETURN n
    " };
    pub const CREATE_ENTITY: Cypher = Cypher { query: "
        CREATE (n:Entity $props) RETURN n
    " };
    pub const SUPERSEDE: Cypher = Cypher { query: "
        MATCH (old {uid: $old_uid})
        SET old.valid_to = $now,
            old.invalidated_at = $now,
            old.invalidated_by = $actor,
            old.invalidated_reason = 'superseded'
        WITH old
        CREATE (new $new_props)
        CREATE (new)-[:SUPERSEDES]->(old)
        RETURN new
    " };
    pub const INVALIDATE: Cypher = Cypher { query: "
        MATCH (n {uid: $uid})
        SET n.valid_to = $now,
            n.invalidated_at = $now,
            n.invalidated_by = $actor,
            n.invalidated_reason = $reason
        RETURN n
    " };
    pub const HARD_PURGE: Cypher = Cypher { query: "
        MATCH (n {uid: $uid}) DETACH DELETE n
    " };
}

pub mod edge {
    use super::Cypher;
    pub const CREATE_GENERIC: Cypher = Cypher { query: "
        MATCH (a {uid: $start_uid}), (b {uid: $end_uid})
        CREATE (a)-[r:$LABEL_PLACEHOLDER $props]->(b) RETURN r
    " };  // NB: $LABEL_PLACEHOLDER replaced via per-label const variants below
}

pub mod traverse {
    use super::Cypher;
    pub const NEIGHBORS_2HOP: Cypher = Cypher { query: "
        MATCH (s {uid: $seed_uid})-[*1..2]-(n)
        WHERE n.valid_to IS NULL
        RETURN DISTINCT n.uid AS uid LIMIT $limit
    " };
}
```

(The `$LABEL_PLACEHOLDER` problem is solved by enumerating per-edge-label CREATE constants — Cypher does not allow parameterized labels.)

### 5e Read-side impl

`crates/moa-memory/graph/src/read.rs`:

```rust
use crate::{age::AgeGraphStore, error::GraphError, node::NodeIndexRow, GraphStore};
use uuid::Uuid;

#[async_trait::async_trait]
impl GraphStore for AgeGraphStore {
    async fn get_node(&self, uid: Uuid) -> Result<Option<NodeIndexRow>, GraphError> {
        // Hot path: fetch from sidecar; FORCE-RLS scopes the result.
        sqlx::query_as!(
            NodeIndexRow,
            r#"SELECT uid, label as "label: _", workspace_id, user_id, scope, name,
                      pii_class as "pii_class: _", valid_to, last_accessed_at
               FROM moa.node_index WHERE uid = $1"#,
            uid,
        ).fetch_optional(&self.pool).await.map_err(GraphError::from)
    }

    async fn neighbors(&self, seed: Uuid, hops: u8, edge_filter: Option<&[crate::edge::EdgeLabel]>) -> Result<Vec<NodeIndexRow>, GraphError> {
        // 1. Fetch neighbor uids via Cypher (uses AGE indexes)
        // 2. Fetch full rows from sidecar (RLS-scoped)
        let limit: i64 = match hops { 1 => 50, 2 => 100, _ => 200 };
        let params = serde_json::json!({"seed_uid": seed.to_string(), "limit": limit});
        let rows: Vec<(serde_json::Value,)> = crate::cypher::traverse::NEIGHBORS_2HOP
            .execute(&params).fetch_all(&self.pool).await?;
        let uids: Vec<Uuid> = rows.into_iter()
            .filter_map(|(v,)| v.get("uid").and_then(|s| s.as_str()).and_then(|s| Uuid::parse_str(s).ok()))
            .collect();
        if uids.is_empty() { return Ok(vec![]); }
        sqlx::query_as!(
            NodeIndexRow,
            r#"SELECT uid, label as "label: _", workspace_id, user_id, scope, name,
                      pii_class as "pii_class: _", valid_to, last_accessed_at
               FROM moa.node_index WHERE uid = ANY($1) AND valid_to IS NULL"#,
            &uids,
        ).fetch_all(&self.pool).await.map_err(Into::into)
    }

    async fn lookup_seeds(&self, name: &str, limit: i64) -> Result<Vec<NodeIndexRow>, GraphError> {
        // delegates to M04 helper (now in this crate)
        crate::node::lookup_seed_by_name(&self.pool, name, limit).await
    }

    // create_node / supersede / invalidate / hard_purge / create_edge: M08 fills these.
    async fn create_node(&self, _: crate::node::NodeWriteIntent) -> Result<Uuid, GraphError> {
        Err(GraphError::NotImplemented("create_node landed in M08"))
    }
    async fn supersede_node(&self, _: Uuid, _: crate::node::NodeWriteIntent) -> Result<Uuid, GraphError> {
        Err(GraphError::NotImplemented("supersede_node landed in M08"))
    }
    async fn invalidate_node(&self, _: Uuid, _: &str) -> Result<(), GraphError> {
        Err(GraphError::NotImplemented("invalidate_node landed in M08"))
    }
    async fn hard_purge(&self, _: Uuid, _: &str) -> Result<(), GraphError> {
        Err(GraphError::NotImplemented("hard_purge landed in M08"))
    }
    async fn create_edge(&self, _: crate::edge::EdgeWriteIntent) -> Result<Uuid, GraphError> {
        Err(GraphError::NotImplemented("create_edge landed in M08"))
    }
}
```

### 5f Error type

```rust
// crates/moa-memory/graph/src/error.rs
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("cypher: {0}")] Cypher(String),
    #[error("sidecar: {0}")] Sidecar(#[from] sqlx::Error),
    #[error("not found: {0}")] NotFound(uuid::Uuid),
    #[error("rls denied")] RlsDenied,
    #[error("not implemented: {0}")] NotImplemented(&'static str),
    #[error("conflict: {0}")] Conflict(String),
    #[error("bi-temporal violation: {0}")] BiTemporal(String),
    #[error(transparent)] Other(#[from] anyhow::Error),
}
```

## 6 Deliverables

- `crates/moa-memory/graph/Cargo.toml`.
- `crates/moa-memory/graph/src/{lib,age,cypher,node,edge,changelog,write,read,error}.rs`.
- Workspace member added.

## 7 Acceptance criteria

1. `cargo build -p moa-memory-graph` clean.
2. Round-trip read test: seed `moa.node_index` with a row, `GraphStore::get_node` returns it.
3. `lookup_seeds("auth")` returns the seeded row.
4. Write methods return `NotImplemented` (will be filled in M08).
5. `cargo doc -p moa-memory-graph` clean.

## 8 Tests

```sh
cargo build -p moa-memory-graph
cargo test -p moa-memory-graph read_smoke
```

## 9 Cleanup

- **Delete any module that hand-rolls openCypher**; that's all here now.
- **Remove `// TODO(M02)` markers** in any consumer that the new `GraphStore::get_node` resolves; those consumers will be wired through retrieval in M15.

## 10 What's next

**M08 — Bi-temporal write protocol (atomic Postgres tx across graph + sidecar + vector + changelog).**
