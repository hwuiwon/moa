# Step M20 — `Connector` trait + `MockConnector` (interfaces only, no real integrations)

_Define the abstract `Connector` trait that all future external-data ingestion paths (Slack, Notion, Google Drive, Linear, Jira, Confluence, GitHub Issues) will implement. Ship `MockConnector` for tests. NO real integrations are built in this prompt — that's a separate workstream._

## 1 What this step is about

External-source ingestion is on the roadmap but not in scope for this pack. We still need the trait now because: (1) M22's S3 audit shipping uses the same `Connector` shape for outbound, (2) the IngestionVO branches on connector type, and (3) future PRs implementing real connectors should drop in without touching consumers.

## 2 Files to read

- M14 (`moa-memory-ingest/src/connector.rs` placeholder)
- M10 (IngestionVO; we'll add a connector-driven entry point)

## 3 Goal

`Connector` trait and supporting types in `moa-memory-ingest`:

```rust
#[async_trait::async_trait]
pub trait Connector: Send + Sync {
    fn id(&self) -> &str;                          // unique connector id, e.g. "slack-acme"
    fn kind(&self) -> ConnectorKind;
    async fn poll(&self, cursor: Option<Cursor>) -> Result<ChangeBatch, ConnectorError>;
    async fn verify_webhook(&self, sig: &str, body: &[u8]) -> Result<bool, ConnectorError>;
    async fn ack(&self, cursor: Cursor) -> Result<(), ConnectorError>;
}

pub enum ConnectorKind { Slack, Notion, Drive, Linear, Jira, Confluence, GithubIssues, Custom }

pub struct ChangeBatch {
    pub items: Vec<ChangeItem>,
    pub next_cursor: Option<Cursor>,
}
pub enum ChangeItem {
    Insert(SourceRecord),
    Update(SourceRecord),
    Delete { external_id: String },
    Tombstone { external_id: String, reason: String },
}
pub struct SourceRecord { /* external_id, title, body, attribution, attached at, ... */ }

pub struct Cursor(pub String);  // opaque to MOA
```

## 4 Rules

- **No real network code**: `MockConnector` returns deterministic test data.
- **Webhook verification trait method** required even if no real webhook is wired — forces future implementations to design signature checks first.
- **Cursor is opaque** to MOA; connectors define semantics.
- **Idempotency** at the consumer side (`moa.connector_state` tracks last_cursor + last_seq per connector).
- **Connectors register themselves** with `moa-runtime::ConnectorRegistry` at startup.

## 5 Tasks

### 5a Migration

`migrations/M20_connector_state.sql`:

```sql
CREATE TABLE moa.connector_state (
    workspace_id     UUID NOT NULL,
    connector_id     TEXT NOT NULL,
    connector_kind   TEXT NOT NULL,
    last_cursor      TEXT,
    last_polled_at   TIMESTAMPTZ,
    error_count      INT NOT NULL DEFAULT 0,
    last_error       TEXT,
    enabled          BOOLEAN NOT NULL DEFAULT true,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, connector_id)
);

ALTER TABLE moa.connector_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.connector_state FORCE ROW LEVEL SECURITY;
CREATE POLICY ws ON moa.connector_state FOR ALL TO moa_app
  USING (workspace_id = moa.current_workspace())
  WITH CHECK (workspace_id = moa.current_workspace());
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.connector_state TO moa_app;
```

### 5b Trait

`crates/moa-memory-ingest/src/connector.rs`:

```rust
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind { Slack, Notion, Drive, Linear, Jira, Confluence, GithubIssues, Custom }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRecord {
    pub external_id: String,
    pub title: Option<String>,
    pub body: String,
    pub attribution: Option<String>,
    pub attached_at: chrono::DateTime<chrono::Utc>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum ChangeItem {
    Insert(SourceRecord),
    Update(SourceRecord),
    Delete { external_id: String },
    Tombstone { external_id: String, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeBatch {
    pub items: Vec<ChangeItem>,
    pub next_cursor: Option<Cursor>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectorError {
    #[error("network: {0}")] Network(String),
    #[error("auth: {0}")] Auth(String),
    #[error("rate-limited; retry after {retry_after_ms}ms")] RateLimited { retry_after_ms: u64 },
    #[error("invalid signature")] InvalidSignature,
    #[error(transparent)] Other(#[from] anyhow::Error),
}

#[async_trait]
pub trait Connector: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> ConnectorKind;
    async fn poll(&self, cursor: Option<Cursor>) -> Result<ChangeBatch, ConnectorError>;
    async fn verify_webhook(&self, sig: &str, body: &[u8]) -> Result<bool, ConnectorError>;
    async fn ack(&self, cursor: Cursor) -> Result<(), ConnectorError>;
}
```

### 5c MockConnector

```rust
pub struct MockConnector {
    pub id: String,
    pub batches: std::sync::Mutex<std::collections::VecDeque<ChangeBatch>>,
}

#[async_trait]
impl Connector for MockConnector {
    fn id(&self) -> &str { &self.id }
    fn kind(&self) -> ConnectorKind { ConnectorKind::Custom }
    async fn poll(&self, _: Option<Cursor>) -> Result<ChangeBatch, ConnectorError> {
        let mut q = self.batches.lock().unwrap();
        Ok(q.pop_front().unwrap_or(ChangeBatch { items: vec![], next_cursor: None }))
    }
    async fn verify_webhook(&self, _: &str, _: &[u8]) -> Result<bool, ConnectorError> { Ok(true) }
    async fn ack(&self, _: Cursor) -> Result<(), ConnectorError> { Ok(()) }
}
```

### 5d Connector-driven IngestionVO entry point

```rust
// in slow_path.rs
#[restate_sdk::object]
impl IngestionVO {
    pub async fn ingest_from_connector(ctx: ObjectContext<'_>, conn_id: String) -> HandlerResult<()> {
        // poll, fan out each ChangeItem as ingest_record (separate handler reusing the chunk→extract→... pipeline)
    }

    pub async fn ingest_record(ctx: ObjectContext<'_>, rec: SourceRecord) -> HandlerResult<()> {
        // chunk the record body, run the same extract/classify/embed/contradict/upsert pipeline
    }
}
```

### 5e Registry stub

`crates/moa-runtime/src/connector_registry.rs`:

```rust
pub struct ConnectorRegistry { connectors: HashMap<String, Arc<dyn Connector>> }
impl ConnectorRegistry {
    pub fn register(&mut self, c: Arc<dyn Connector>) { /* ... */ }
    pub fn get(&self, id: &str) -> Option<Arc<dyn Connector>> { /* ... */ }
}
```

## 6 Deliverables

- `migrations/M20_connector_state.sql`.
- `crates/moa-memory-ingest/src/connector.rs` (~250 lines).
- `crates/moa-memory-ingest/src/connector_mock.rs`.
- `crates/moa-runtime/src/connector_registry.rs`.
- New IngestionVO handlers `ingest_from_connector` / `ingest_record`.

## 7 Acceptance criteria

1. `cargo build -p moa-memory-ingest` clean; trait compiles.
2. MockConnector test: enqueue 3 batches, poll returns them in order then empty.
3. `ingest_from_connector` end-to-end with MockConnector lands records as Source nodes (or Fact nodes — TBD by record kind).
4. `connector_state.last_cursor` advances after ack.

## 8 Tests

```sh
cargo test -p moa-memory-ingest mock_connector
cargo test -p moa-memory-ingest ingest_from_connector_e2e
```

## 9 Cleanup

- No prior connector code; no cleanup needed.
- Remove any `// TODO: connector` comments in IngestionVO.

## 10 What's next

**M21 — KEK/DEK envelope encryption layer in `moa-security`.**
