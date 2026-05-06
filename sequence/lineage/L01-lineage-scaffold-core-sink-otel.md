# Step L01 — Scaffold `crates/moa-lineage/` + engineering-tier capture

_Build the parent folder, three subcrates (core, sink, otel), the TimescaleDB hot store, the async dual-write pipeline (mpsc → fjall → COPY), the OTel + OpenInference attribute emitters, and wire one `LineageSink` into `ToolContext` so the orchestrator emits retrieval / context / generation lineage on every turn. After this prompt, `moa explain <session>` and `moa retrieve --debug` work end-to-end against a populated TimescaleDB hypertable._

## 1 What this step is about

Today MOA emits OpenTelemetry traces but no structured retrieval lineage, no per-turn context manifest, and no provenance trail. There's nothing to query when an answer is wrong. L01 lays down the data layer so that every turn produces durable, queryable, async-captured lineage records — without adding latency to the hot path.

L01 covers **engineering tier only**. Citations beyond vendor passthrough, cold-tier Parquet, eval scores, and compliance audit ship in L02–L04. The split is deliberate: schema instability between phases is expensive, so L01 should ship and run for a few days before L02 lands on top.

## 2 Files to read

- C00–C06 prompts (cutover history — explains current `ToolContext` shape and graph-stack handle traits in `moa-core`)
- `crates/moa-core/src/traits.rs` — the trait surfaces the new sink slots into
- `crates/moa-orchestrator-local/src/lib.rs` (and `crates/moa-orchestrator/` if applicable) — the call sites that need to emit
- `crates/moa-brain/src/pipeline/` — retriever entry point and stages
- `crates/moa-memory/graph/src/lib.rs` — `GraphStore` trait, what graph paths look like for capture
- `crates/moa-memory/vector/src/lib.rs` — `Embedder` and vector-store APIs (relevant to retrieval introspection)
- `crates/moa-providers/` — every LLM provider wrapper (Anthropic, OpenAI, Cohere, Vertex) — generation-lineage emit lives here
- M22 docs (pgaudit + S3 Object Lock) for context on what the compliance tier in L04 will reuse
- This pack's `README-LINEAGE.md`

## 3 Goal

After L01:

- `crates/moa-lineage/{core,sink,otel}/` exist with the three subcrates and a parent `README.md`.
- TimescaleDB extension is enabled on the existing Postgres 17 cluster; `analytics.turn_lineage` hypertable + indexes + continuous aggregate exist.
- `LineageSink` trait is defined in `moa-lineage-core`.
- `MpscSink` (the production sink) and `NullSink` (the disabled-cost fallback) are implemented in `moa-lineage-sink`.
- The async writer worker (mpsc → fjall durable journal → batch COPY into TimescaleDB) is running as a tokio task spawned by the orchestrator.
- `ToolContext` carries a `&dyn LineageSink` (added via the same handle-trait bridge pattern as the graph stack).
- The retriever, context compiler, and LLM provider wrappers emit real `RetrievalLineage`, `ContextLineage`, and `GenerationLineage` events.
- OTel GenAI v1.38 + OpenInference attributes are emitted in parallel through `tracing-opentelemetry`.
- `moa explain <session-id>` reads from the hypertable and prints a turn-by-turn tree.
- `moa retrieve --debug "<query>"` runs retrieval with capture enabled and prints the full ranking trace.
- `cargo build --workspace` clean. `cargo test --workspace` green.

## 4 Rules

- **Folder grouping pattern matches `crates/moa-memory/`.** Parent folder, sibling subcrates, separate package names. Do NOT collapse to a single crate. Do NOT add a `mod.rs` aggregator at the parent level.
- **No async work on the hot path.** `LineageSink::record` is a `&self` non-`async` method that does at most one `try_send` and returns. If the channel is full, drop the event and increment a counter. Never block, never await, never allocate beyond the event itself.
- **Async writer is spawned once at orchestrator startup** and lives for the orchestrator's lifetime. Graceful shutdown drains the channel and flushes fjall + COPY.
- **At-least-once.** Idempotency on `(turn_id, record_kind, ts)`. `ON CONFLICT DO UPDATE` handles late-arriving enrichments (citation arriving after generation, etc.) — this matters in L02 but the schema accommodates it now.
- **OTel GenAI v1.38 attribute names — exact spelling matters.** No `gen_ai.prompt` / `gen_ai.completion` (deprecated). Use `gen_ai.input.messages` / `gen_ai.output.messages` and gate full content on `MOA_CAPTURE_MESSAGE_CONTENT=true`.
- **OpenInference dual-emit.** Phoenix users expect `openinference.span.kind`, `retrieval.documents.<i>.document.{id,content,score,metadata}`. Emit both namespaces — the storage cost is trivial.
- **Type-stability is in `moa-lineage-core`**. The other subcrates depend on core but never on each other through types. `sink` knows about core's records; `otel` knows about core's records; they don't know about each other.
- **`ToolContext` integration uses the same thin-trait bridge pattern as the graph stack** (added in C05). Do not import `moa-lineage-core` directly into `moa-core`. Define a `LineageHandle` trait in `moa-core` and impl it for `MpscSink`/`NullSink` in `moa-lineage-sink`.
- **No FileMemoryStore-shaped APIs.** Lineage records are append-only; no read_page-equivalent.

## 5 Tasks

### 5a Create the folder skeleton

```sh
mkdir -p crates/moa-lineage
mkdir -p crates/moa-lineage/{core,sink,otel}/src
touch crates/moa-lineage/core/src/lib.rs
touch crates/moa-lineage/sink/src/lib.rs
touch crates/moa-lineage/otel/src/lib.rs
```

Add `crates/moa-lineage/README.md` (~15 lines, sibling pattern to `crates/moa-memory/README.md`):

```markdown
# moa-lineage

Two-tier observability + explainability for MOA. The subcrates here form one logical
unit; they're separated to keep the hot path (`sink`), the wire format (`otel`), and the
record shapes (`core`) independently versionable. `citation/`, `cold/`, and `audit/`
will be added by L02–L04.

## Subcrates

| Path        | Crate name              | Responsibility |
|-------------|-------------------------|----------------|
| `core/`     | `moa-lineage-core`      | `LineageSink` trait; record shapes; scope/ID types; serde wire format |
| `sink/`     | `moa-lineage-sink`      | mpsc + fjall durable journal + COPY-bulk TimescaleDB writer + worker lifecycle |
| `otel/`     | `moa-lineage-otel`      | OTel GenAI v1.38 + OpenInference attribute emitters; tracing-bridge |
| `citation/` | `moa-lineage-citation`  | (L02) vendor passthrough adapters + cascade NLI verifier |
| `cold/`     | `moa-lineage-cold`      | (L02) Parquet + S3 exporter; retention policy |
| `audit/`    | `moa-lineage-audit`     | (L04) BLAKE3 hash chain + ct-merkle + Object Lock + PII HMAC vault |

## Public surface

- `moa_lineage_core::{LineageSink, LineageEvent, RetrievalLineage, ContextLineage, GenerationLineage, TurnId}`
- `moa_lineage_sink::{MpscSink, NullSink, SinkConfig, SinkHandle}`
- `moa_lineage_otel::{emit_retrieval_attrs, emit_generation_attrs, emit_context_attrs}`

The CLI subcommands (`moa explain`, `moa retrieve --debug`, `moa lineage query`,
`moa lineage export`) live in `crates/moa-cli/`.

## Phase status

L01 ships core + sink + otel; L02 adds citation + cold; L03 wires eval and dashboards;
L04 adds compliance audit. See `sequence/L*-*.md` for prompts.
```

### 5b Add `moa-lineage-core/Cargo.toml`

```toml
[package]
name = "moa-lineage-core"
version = "0.1.0"
edition = "2024"

[dependencies]
moa-core = { path = "../../moa-core" }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
uuid = { workspace = true }
chrono = { workspace = true, features = ["serde"] }
async-trait = { workspace = true }
thiserror = { workspace = true }
```

### 5c `moa-lineage-core/src/lib.rs`

```rust
//! Core lineage data model and sink trait.
//!
//! This crate is the type-stable foundation for moa-lineage. Other subcrates
//! (sink, otel, citation, cold, audit) depend on it; it depends only on
//! moa-core for shared identity types.

pub mod records;
pub mod sink;
pub mod ids;

pub use ids::{TurnId, LineageRecordId};
pub use records::{
    LineageEvent,
    RetrievalLineage, RetrievalStage, VecHit, GraphPath, FusedHit, RerankHit,
    ContextLineage, ContextChunk, TruncationEvent,
    GenerationLineage, ToolCallSummary, TokenUsage,
    StageTimings, BackendIntrospection,
    PgvectorIntrospection, AgeIntrospection, TurbopufferIntrospection,
    RecordKind,
};
pub use sink::{LineageSink, NullSink};
```

### 5d Define the records (`moa-lineage-core/src/records.rs`)

The shape below is the authoritative serialization for L01. Stage 1 ships only `Retrieval`, `Context`, `Generation`. `Citation`, `Eval`, `Decision` variants are reserved for L02/L03/L04 — include them in the enum so the storage schema doesn't churn, but leave bodies as `serde_json::Value` placeholders.

```rust
use chrono::{DateTime, Utc};
use moa_core::{MemoryScope, SessionId, UserId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;
use uuid::Uuid;

use crate::ids::TurnId;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LineageEvent {
    Retrieval(RetrievalLineage),
    Context(ContextLineage),
    Generation(GenerationLineage),
    /// Reserved — emitted by `moa-lineage-citation` in L02.
    Citation(serde_json::Value),
    /// Reserved — emitted by `moa-eval-lineage` in L03.
    Eval(serde_json::Value),
    /// Reserved — emitted by `moa-lineage-audit` in L04.
    Decision(serde_json::Value),
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[repr(i16)]
pub enum RecordKind {
    Retrieval  = 1,
    Context    = 2,
    Generation = 3,
    Citation   = 4,
    Eval       = 5,
    Decision   = 6,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalLineage {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub scope: MemoryScope,
    pub ts: DateTime<Utc>,
    pub query_original: String,
    pub query_expansions: Vec<String>,
    pub vector_hits: Vec<VecHit>,
    pub graph_paths: Vec<GraphPath>,
    pub fusion_scores: Vec<FusedHit>,
    pub rerank_scores: Vec<RerankHit>,
    pub top_k: Vec<Uuid>,                     // chunk_ids that survived to context
    pub timings: StageTimings,
    pub introspection: BackendIntrospection,
    pub stage: RetrievalStage,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum RetrievalStage {
    Single,                    // single hybrid retrieve
    SubQuery { idx: usize },   // multi-step retrieval planner
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VecHit {
    pub chunk_id: Uuid,
    pub score: f32,
    pub source: String,        // "pgvector" | "turbopuffer"
    pub embedder: String,      // "cohere-embed-v4" | "gemini-embedding-2" | "gemini-embedding-001"
    pub embed_dim: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphPath {
    pub start: Uuid,
    pub end: Uuid,
    pub edges: Vec<Uuid>,
    pub labels: Vec<String>,
    pub length: u8,
    pub score: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FusedHit {
    pub chunk_id: Uuid,
    pub fused_score: f32,
    pub vector_contribution: f32,
    pub graph_contribution: f32,
    pub lexical_contribution: f32,
    pub fusion_method: String,  // "rrf" | "weighted_sum" | "linear"
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RerankHit {
    pub chunk_id: Uuid,
    pub original_index: u16,
    pub relevance_score: f32,
    pub rerank_model: String,   // e.g. "rerank-v4.0-fast"
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageTimings {
    pub embed_ms: u32,
    pub vector_search_ms: u32,
    pub graph_search_ms: u32,
    pub lexical_search_ms: u32,
    pub fusion_ms: u32,
    pub rerank_ms: u32,
    pub total_ms: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BackendIntrospection {
    pub pgvector: Option<PgvectorIntrospection>,
    pub age: Option<AgeIntrospection>,
    pub turbopuffer: Option<TurbopufferIntrospection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PgvectorIntrospection {
    pub ef_search: u32,
    pub iterative_scan: Option<String>, // "strict_order" | "relaxed_order"
    pub buffers_hit: Option<u64>,
    pub buffers_read: Option<u64>,
    pub planning_ms: Option<f32>,
    pub execution_ms: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgeIntrospection {
    pub max_path_length: u8,
    pub edges_walked: u32,
    pub paths_returned: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurbopufferIntrospection {
    pub namespace: String,
    pub consistency: String,
    pub billed_units: Option<f64>,
    pub client_wall_clock_ms: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextLineage {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub ts: DateTime<Utc>,
    pub chunks_in_window: Vec<ContextChunk>,
    pub truncations: Vec<TruncationEvent>,
    pub prefix_cache_hit_tokens: Option<u32>,
    pub prefix_cache_miss_tokens: Option<u32>,
    pub total_input_tokens_estimated: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextChunk {
    pub chunk_id: Uuid,
    pub source_uid: Uuid,
    pub position: u16,
    pub estimated_tokens: u32,
    pub role: String,                // "system" | "user" | "assistant" | "tool" | "context"
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TruncationEvent {
    pub chunk_id: Option<Uuid>,
    pub reason: String,              // "token_budget" | "policy" | "ranking_floor"
    pub tokens_dropped: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenerationLineage {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub ts: DateTime<Utc>,
    pub provider: String,            // "anthropic" | "openai" | "cohere" | "vertex"
    pub request_model: String,
    pub response_model: String,
    pub usage: TokenUsage,
    pub finish_reasons: Vec<String>,
    pub tool_calls: Vec<ToolCallSummary>,
    pub cost_micros: u64,            // 1e-6 USD; avoids float
    pub duration: Duration,
    pub trace_id: Option<String>,    // OTel trace_id for cross-reference
    pub span_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: Option<u32>,
    pub cache_creation_tokens: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub tool_name: String,
    pub call_id: String,
    pub argument_size_bytes: u32,
    pub result_size_bytes: u32,
    pub duration: Duration,
    pub error: Option<String>,
}

impl LineageEvent {
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            LineageEvent::Retrieval(r)  => Some(r.turn_id),
            LineageEvent::Context(c)    => Some(c.turn_id),
            LineageEvent::Generation(g) => Some(g.turn_id),
            _ => None,
        }
    }
    pub fn record_kind(&self) -> RecordKind {
        match self {
            LineageEvent::Retrieval(_)  => RecordKind::Retrieval,
            LineageEvent::Context(_)    => RecordKind::Context,
            LineageEvent::Generation(_) => RecordKind::Generation,
            LineageEvent::Citation(_)   => RecordKind::Citation,
            LineageEvent::Eval(_)       => RecordKind::Eval,
            LineageEvent::Decision(_)   => RecordKind::Decision,
        }
    }
}
```

### 5e Define the sink trait (`moa-lineage-core/src/sink.rs`)

```rust
use crate::records::LineageEvent;

/// Hot-path tap. Cheap to call, never blocks.
///
/// Implementations MUST NOT await, allocate beyond the event payload, or
/// perform IO synchronously. `record` returns immediately; capture is
/// performed by an async worker owned by the implementation.
pub trait LineageSink: Send + Sync + 'static {
    /// Records an event. Drops silently with a counter increment if the
    /// implementation's buffer is saturated.
    fn record(&self, evt: LineageEvent);

    /// Returns true if any events have been dropped due to buffer pressure.
    /// Wired into the `moa_lineage_dropped_total` Prometheus metric.
    fn dropped_count(&self) -> u64;
}

/// Disabled-cost fallback. `record` is one comparison + branch.
pub struct NullSink;

impl LineageSink for NullSink {
    fn record(&self, _evt: LineageEvent) {}
    fn dropped_count(&self) -> u64 { 0 }
}
```

### 5f `moa-lineage-core/src/ids.rs`

```rust
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One TurnId per agent turn. Generated at the orchestrator's turn boundary.
/// All lineage records for that turn share this ID.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub Uuid);

impl TurnId {
    pub fn new_v7() -> Self { Self(Uuid::now_v7()) }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LineageRecordId(pub Uuid);
```

### 5g Add `LineageHandle` to `moa-core`

In `crates/moa-core/src/traits.rs`, add a thin handle trait — same pattern as `GraphReadHandle` from C05:

```rust
/// Hot-path observability tap. Implemented by `moa-lineage-sink::MpscSink`.
/// `moa-core` defines the trait so `ToolContext` can carry a reference
/// without depending on the lineage crate (avoids circular deps).
pub trait LineageHandle: Send + Sync {
    fn record(&self, evt_json: serde_json::Value);
}
```

Add the field to `ToolContext`:

```rust
pub struct ToolContext<'a> {
    pub session: &'a SessionMeta,
    pub graph: &'a dyn GraphReadHandle,
    pub retriever: &'a dyn RetrievalHandle,
    pub ingestion: &'a dyn IngestHandle,
    pub lineage: &'a dyn LineageHandle,   // NEW
    pub session_store: Option<&'a dyn SessionStore>,
    pub cancel_token: Option<&'a CancellationToken>,
}
```

A no-op `NullLineageHandle` lives in `moa-core` so tests and tools that don't care about lineage don't need to depend on the lineage crate.

### 5h `moa-lineage-sink/Cargo.toml`

```toml
[package]
name = "moa-lineage-sink"
version = "0.1.0"
edition = "2024"

[dependencies]
moa-core         = { path = "../../moa-core" }
moa-lineage-core = { path = "../core" }
tokio          = { workspace = true, features = ["sync", "rt", "time", "macros"] }
tokio-postgres = { workspace = true }
deadpool-postgres = { workspace = true }
fjall          = "3"
serde          = { workspace = true, features = ["derive"] }
serde_json     = { workspace = true }
uuid           = { workspace = true }
chrono         = { workspace = true }
metrics        = { workspace = true }
tracing        = { workspace = true }
thiserror      = { workspace = true }
async-trait    = { workspace = true }
```

### 5i Implement `MpscSink` (`moa-lineage-sink/src/lib.rs`)

```rust
//! mpsc → fjall → COPY-bulk TimescaleDB writer.

mod mpsc_sink;
mod writer;
mod fjall_journal;
mod schema;

pub use mpsc_sink::{MpscSink, MpscSinkConfig, MpscSinkBuilder};
pub use writer::{LineageWriter, WriterStats};
pub use schema::ensure_schema;
```

`mpsc_sink.rs`:

```rust
use moa_core::traits::LineageHandle;
use moa_lineage_core::{LineageEvent, LineageSink};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct MpscSinkConfig {
    /// Channel depth. 8K is the recommended default.
    pub channel_capacity: usize,
    /// Worker batch flush window.
    pub batch_size: usize,
    /// Worker batch max age.
    pub batch_max_age: std::time::Duration,
    /// fjall journal directory.
    pub journal_path: std::path::PathBuf,
}

impl Default for MpscSinkConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 8192,
            batch_size: 512,
            batch_max_age: std::time::Duration::from_secs(2),
            journal_path: "/var/lib/moa/lineage-journal".into(),
        }
    }
}

#[derive(Clone)]
pub struct MpscSink {
    tx: mpsc::Sender<LineageEvent>,
    dropped: Arc<AtomicU64>,
}

impl MpscSink {
    /// Spawns the worker task; returns the sink handle.
    /// Caller must keep a reference to the returned `WriterHandle` to
    /// trigger graceful shutdown.
    pub async fn spawn(
        config: MpscSinkConfig,
        pool: deadpool_postgres::Pool,
    ) -> anyhow::Result<(Self, crate::writer::WriterHandle)> {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_handle = crate::writer::spawn_writer(rx, config, pool).await?;
        Ok((Self { tx, dropped }, writer_handle))
    }
}

impl LineageSink for MpscSink {
    fn record(&self, evt: LineageEvent) {
        if let Err(_) = self.tx.try_send(evt) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("moa_lineage_dropped_total").increment(1);
        } else {
            metrics::counter!("moa_lineage_recorded_total").increment(1);
        }
    }
    fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// Bridge: `LineageHandle` is the trait `moa-core` defines for `ToolContext`.
// We accept JSON here so the bridge avoids depending on lineage records in
// moa-core itself.
impl LineageHandle for MpscSink {
    fn record(&self, evt_json: serde_json::Value) {
        match serde_json::from_value::<LineageEvent>(evt_json) {
            Ok(evt) => LineageSink::record(self, evt),
            Err(e) => tracing::warn!("malformed lineage event: {e}"),
        }
    }
}
```

`writer.rs` is the heart — implements:

1. Spawn a tokio task that loops on `rx.recv_many(buf, batch_size)`.
2. For each batch: append to fjall transactional keyspace (durable enqueue), then COPY-bulk into `analytics.turn_lineage`.
3. On COPY success: delete the fjall keys (acknowledged).
4. On COPY failure: leave them in fjall, sleep with exponential backoff, retry.
5. On graceful shutdown signal (`CancellationToken`): drain the channel, flush remaining batch, close fjall.
6. On startup: enumerate stale fjall entries (from prior crash) and replay them through COPY before processing the new mpsc receiver.

Use `tokio_postgres::CopyInSink` with the binary COPY format. Encode each row as `(turn_id::uuid, session_id::uuid, user_id::uuid, workspace_id::uuid, ts::timestamptz, tier::int2, record_kind::int2, payload::jsonb, integrity_hash::bytea, prev_hash::bytea_nullable)`. For L01, `tier=1`, `prev_hash=NULL`, `integrity_hash=BLAKE3(payload_canonical_json)`. The hash is engineering-tier "free" insurance — it's not a chain yet, but L04 will only need to add the chain link on top.

`fjall_journal.rs`:

```rust
use fjall::{Config, TransactionalKeyspace, PartitionCreateOptions};

pub struct Journal {
    keyspace: TransactionalKeyspace,
    partition: fjall::TxPartitionHandle,
}

impl Journal {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let keyspace = Config::new(path).open_transactional()?;
        let partition = keyspace.open_partition(
            "lineage-pending",
            PartitionCreateOptions::default(),
        )?;
        Ok(Self { keyspace, partition })
    }

    pub fn append(&self, seq: u64, payload: &[u8]) -> anyhow::Result<()> {
        let mut tx = self.keyspace.write_tx()?;
        tx.insert(&self.partition, seq.to_be_bytes(), payload);
        tx.commit()?;
        Ok(())
    }

    pub fn ack_range(&self, lo: u64, hi: u64) -> anyhow::Result<()> {
        let mut tx = self.keyspace.write_tx()?;
        for seq in lo..=hi {
            tx.remove(&self.partition, seq.to_be_bytes());
        }
        tx.commit()?;
        Ok(())
    }

    pub fn replay(&self) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
        let mut out = Vec::new();
        for kv in self.partition.iter() {
            let (k, v) = kv?;
            let seq = u64::from_be_bytes(k.as_ref().try_into()?);
            out.push((seq, v.to_vec()));
        }
        Ok(out)
    }
}
```

### 5j `schema.rs` — TimescaleDB DDL

```rust
pub const SCHEMA_DDL: &str = include_str!("../sql/schema.sql");

pub async fn ensure_schema(client: &tokio_postgres::Client) -> anyhow::Result<()> {
    client.batch_execute(SCHEMA_DDL).await?;
    Ok(())
}
```

`crates/moa-lineage/sink/sql/schema.sql`:

```sql
-- Idempotent. Run on startup.
CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TABLE IF NOT EXISTS analytics.turn_lineage (
    turn_id        UUID         NOT NULL,
    session_id     UUID         NOT NULL,
    user_id        UUID         NOT NULL,
    workspace_id   UUID         NOT NULL,
    ts             TIMESTAMPTZ  NOT NULL,
    tier           SMALLINT     NOT NULL DEFAULT 1,
    record_kind    SMALLINT     NOT NULL,
    payload        JSONB        NOT NULL,
    answer_text    TEXT,
    integrity_hash BYTEA        NOT NULL,
    prev_hash      BYTEA,
    PRIMARY KEY (turn_id, record_kind, ts)
);

SELECT create_hypertable(
    'analytics.turn_lineage',
    'ts',
    chunk_time_interval => INTERVAL '1 day',
    if_not_exists => TRUE
);

ALTER TABLE analytics.turn_lineage SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'workspace_id',
    timescaledb.compress_orderby   = 'ts DESC, turn_id'
);

SELECT add_compression_policy('analytics.turn_lineage', INTERVAL '7 days', if_not_exists => TRUE);
SELECT add_retention_policy   ('analytics.turn_lineage', INTERVAL '30 days', if_not_exists => TRUE);

CREATE INDEX IF NOT EXISTS ix_lineage_session_ts
    ON analytics.turn_lineage (session_id, ts DESC);

CREATE INDEX IF NOT EXISTS ix_lineage_workspace_user_ts
    ON analytics.turn_lineage (workspace_id, user_id, ts DESC);

CREATE INDEX IF NOT EXISTS ix_lineage_zero_recall
    ON analytics.turn_lineage (ts DESC)
    WHERE record_kind = 1
      AND jsonb_array_length(payload #> '{retrieval,top_k}') = 0;

CREATE INDEX IF NOT EXISTS ix_lineage_payload_gin
    ON analytics.turn_lineage
    USING GIN ((payload) jsonb_path_ops);

-- Continuous aggregate for the operator dashboard (L03 will lean on it).
CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.turn_recall_hourly
WITH (timescaledb.continuous) AS
SELECT time_bucket('1 hour', ts) AS bucket,
       workspace_id,
       COUNT(*) AS turns,
       COUNT(*) FILTER (
           WHERE record_kind = 1
             AND jsonb_array_length(payload #> '{retrieval,top_k}') = 0
       ) AS zero_recall
FROM analytics.turn_lineage
GROUP BY bucket, workspace_id
WITH NO DATA;

SELECT add_continuous_aggregate_policy('analytics.turn_recall_hourly',
    start_offset => INTERVAL '7 days',
    end_offset   => INTERVAL '5 minutes',
    schedule_interval => INTERVAL '5 minutes',
    if_not_exists => TRUE);
```

### 5k `moa-lineage-otel/Cargo.toml`

```toml
[package]
name = "moa-lineage-otel"
version = "0.1.0"
edition = "2024"

[dependencies]
moa-lineage-core = { path = "../core" }
opentelemetry = { workspace = true }
tracing = { workspace = true }
tracing-opentelemetry = { workspace = true }
openinference-semantic-conventions = "*"   # pin in workspace.dependencies; latest as of pack
serde_json = { workspace = true }
```

### 5l `moa-lineage-otel/src/lib.rs`

```rust
//! OpenTelemetry GenAI semantic conventions v1.38 + OpenInference attribute
//! emission. This crate is a *parallel* surface to MpscSink — it does not
//! replace the durable Postgres writer. Use both: spans for ad-hoc trace
//! debugging in Jaeger/Tempo, the hypertable for SQL queries, dashboards,
//! and audit.

use moa_lineage_core::{
    ContextLineage, GenerationLineage, RetrievalLineage,
};
use tracing::{Span, field};

pub fn emit_retrieval_attrs(span: &Span, r: &RetrievalLineage) {
    span.record("gen_ai.operation.name", "retrieval");
    span.record("gen_ai.data_source.id", r.vector_hits.first()
        .map(|v| v.source.as_str()).unwrap_or("unknown"));
    span.record("openinference.span.kind", "RETRIEVER");
    // OTel structured form
    if let Ok(json) = serde_json::to_string(&r.top_k) {
        span.record("gen_ai.retrieval.documents", field::display(&json));
    }
    // OpenInference flat form (what Phoenix consumes)
    for (i, hit) in r.vector_hits.iter().take(20).enumerate() {
        let key_id = format!("retrieval.documents.{i}.document.id");
        let key_score = format!("retrieval.documents.{i}.document.score");
        let key_meta = format!("retrieval.documents.{i}.document.metadata");
        span.record(key_id.as_str(),     field::display(&hit.chunk_id));
        span.record(key_score.as_str(),  field::display(&hit.score));
        span.record(key_meta.as_str(),
            field::display(&format!(
                "{{\"source\":\"{}\",\"embedder\":\"{}\"}}",
                hit.source, hit.embedder
            )));
    }
    // pgvector / age / turbopuffer specific (MOA namespace)
    if let Some(pg) = &r.introspection.pgvector {
        span.record("moa.pgvector.ef_search", pg.ef_search);
        if let Some(b) = pg.buffers_hit {
            span.record("moa.pgvector.buffers_hit", b);
        }
    }
    if let Some(age) = &r.introspection.age {
        span.record("moa.age.path_length", age.max_path_length);
        span.record("moa.age.edges_walked", age.edges_walked);
    }
    if let Some(tp) = &r.introspection.turbopuffer {
        span.record("moa.tpuf.namespace", tp.namespace.as_str());
        if let Some(billed) = tp.billed_units {
            span.record("moa.tpuf.billed_units", billed);
        }
    }
    span.record("moa.retrieval.total_ms", r.timings.total_ms);
}

pub fn emit_context_attrs(span: &Span, c: &ContextLineage) {
    span.record("gen_ai.operation.name", "context_compile");
    span.record("openinference.span.kind", "CHAIN");
    span.record("moa.context.chunks_in_window", c.chunks_in_window.len() as u64);
    span.record("moa.context.truncations", c.truncations.len() as u64);
    if let Some(hit) = c.prefix_cache_hit_tokens {
        span.record("gen_ai.usage.cache_read.input_tokens", hit);
    }
    if let Some(miss) = c.prefix_cache_miss_tokens {
        span.record("gen_ai.usage.cache_creation.input_tokens", miss);
    }
}

pub fn emit_generation_attrs(span: &Span, g: &GenerationLineage) {
    span.record("gen_ai.operation.name", "chat");
    span.record("openinference.span.kind", "LLM");
    span.record("gen_ai.provider.name", g.provider.as_str());
    span.record("gen_ai.request.model", g.request_model.as_str());
    span.record("gen_ai.response.model", g.response_model.as_str());
    span.record("gen_ai.usage.input_tokens", g.usage.input_tokens);
    span.record("gen_ai.usage.output_tokens", g.usage.output_tokens);
    if let Some(cr) = g.usage.cache_read_tokens {
        span.record("gen_ai.usage.cache_read.input_tokens", cr);
    }
    if let Some(cc) = g.usage.cache_creation_tokens {
        span.record("gen_ai.usage.cache_creation.input_tokens", cc);
    }
    span.record("gen_ai.response.finish_reasons",
        field::display(&g.finish_reasons.join(",")));
    span.record("gen_ai.conversation.id", field::display(&g.session_id));
    span.record("moa.cost_micros", g.cost_micros);
}
```

### 5m Update workspace `Cargo.toml`

Add the three subcrates to `members`:

```toml
"crates/moa-lineage/core",
"crates/moa-lineage/sink",
"crates/moa-lineage/otel",
```

(L02 will add citation/cold; L04 will add audit. Don't add them yet — the agent should fail loudly if it tries to import an unbuilt crate.)

### 5n Wire the sink into orchestrator startup

In `crates/moa-orchestrator-local/src/lib.rs`, at the construction site where the graph stack handles are built (post-C03), add:

```rust
use moa_lineage_sink::{MpscSink, MpscSinkConfig, ensure_schema};

let lineage_sink_handle: Arc<dyn LineageHandle> = match &config.observability.lineage {
    LineageConfig::Disabled => Arc::new(NullLineageHandle),
    LineageConfig::Enabled(cfg) => {
        // Ensure schema (idempotent).
        let mut conn = pool.get().await?;
        ensure_schema(&conn).await?;
        let (sink, writer_handle) = MpscSink::spawn(cfg.into(), pool.clone()).await?;
        // Stash writer_handle on orchestrator so graceful shutdown drains it.
        self.lineage_writer_handles.push(writer_handle);
        Arc::new(sink) as Arc<dyn LineageHandle>
    }
};
```

Pass `lineage_sink_handle` into every `ToolContext` construction.

### 5o Emit at the call sites

Find each of these and add a single emission. Don't refactor the call paths beyond the minimum.

**Retriever (`crates/moa-brain/src/pipeline/retriever.rs` or wherever the hybrid retriever fan-in lands):**

```rust
let retrieval = RetrievalLineage {
    turn_id: ctx.turn_id,
    session_id: ctx.session.session_id,
    workspace_id: ctx.session.workspace_id.clone(),
    user_id: ctx.session.user_id.clone(),
    scope: scope.clone(),
    ts: Utc::now(),
    query_original: query.to_string(),
    query_expansions: expansions.clone(),
    vector_hits: vec_hits_clone,
    graph_paths: graph_paths_clone,
    fusion_scores: fused.clone(),
    rerank_scores: rerank_results.clone(),
    top_k: final_top_k.iter().map(|h| h.chunk_id).collect(),
    timings,
    introspection,
    stage: RetrievalStage::Single,
};

let json = serde_json::to_value(LineageEvent::Retrieval(retrieval.clone()))?;
ctx.lineage.record(json);

// Also emit OTel attributes onto the current span.
moa_lineage_otel::emit_retrieval_attrs(&Span::current(), &retrieval);
```

**Context compiler (`crates/moa-brain/src/pipeline/context.rs`):** same pattern, emit `ContextLineage` after the compiler finalizes its output.

**LLM provider wrappers (`crates/moa-providers/src/{anthropic,openai,cohere,vertex}.rs`):** after each provider response, emit `GenerationLineage` from inside the wrapper. The wrapper has all the data — usage, model, finish reasons, tool calls, duration, the OTel `trace_id` of the surrounding span.

`pgvector` introspection: wrap the search SQL in `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` on a sampled fraction (1%), parse `Buffers: shared hit/read` and `Planning Time` / `Execution Time` into `PgvectorIntrospection`. On the non-sampled 99%, only `ef_search` from the SET command is captured.

`AGE` introspection: add `MATCH path = ...` to the existing Cypher; capture path length, edges walked, paths returned. Don't try to call `PROFILE` — AGE doesn't support it.

`Turbopuffer` introspection: from the `query()` response, capture namespace, consistency, billed_units; client wall-clock from `Instant::now()` deltas.

### 5p Add `moa-cli` subcommands

Two new commands. The implementation reads from the hypertable directly via `tokio-postgres`.

**`moa explain <session-id>`:**

```rust
async fn explain_session(config: &MoaConfig, session_id: SessionId) -> Result<String> {
    let pool = config.lineage_pool().await?;
    let conn = pool.get().await?;
    let rows = conn.query(
        "SELECT turn_id, ts, record_kind, payload
           FROM analytics.turn_lineage
          WHERE session_id = $1
          ORDER BY ts ASC, record_kind ASC",
        &[&session_id.0]
    ).await?;
    let mut out = String::new();
    let mut last_turn: Option<Uuid> = None;
    for row in rows {
        let turn_id: Uuid = row.get(0);
        let ts: chrono::DateTime<Utc> = row.get(1);
        let kind: i16 = row.get(2);
        let payload: serde_json::Value = row.get(3);
        if Some(turn_id) != last_turn {
            out.push_str(&format!("\n=== turn {turn_id}  {ts}\n"));
            last_turn = Some(turn_id);
        }
        match kind {
            1 => render_retrieval(&payload, &mut out),
            2 => render_context(&payload, &mut out),
            3 => render_generation(&payload, &mut out),
            _ => {}
        }
    }
    Ok(out)
}
```

`moa explain <turn-id>` is a turn-scoped variant: same query, one turn.

**`moa retrieve --debug "<query>"`:**

```rust
async fn debug_retrieve(config: &MoaConfig, query: &str) -> Result<String> {
    let retriever = load_hybrid_retriever(config).await?;
    let scope = MemoryScope::Workspace { workspace_id: current_workspace_id() };
    // Capture the lineage for this synthetic turn, even though no LLM call
    // follows. The retriever emits a RetrievalLineage as it normally does.
    let synthetic_turn = TurnId::new_v7();
    let _hits = retriever.retrieve_with_turn(query, &scope, 10, synthetic_turn).await?;

    // Pull the just-emitted lineage from the hypertable. The async writer
    // may take up to ~2 seconds (batch_max_age) to flush; poll up to 5s.
    for _ in 0..50 {
        let rows = pool.get().await?.query(
            "SELECT payload FROM analytics.turn_lineage \
              WHERE turn_id = $1 AND record_kind = 1 LIMIT 1",
            &[&synthetic_turn.0]
        ).await?;
        if let Some(row) = rows.first() {
            let payload: serde_json::Value = row.get(0);
            return Ok(format_retrieval_debug(&payload));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("lineage write did not flush within 5s");
}
```

For interactive UX `--debug` should also accept a `--no-flush-wait` mode that prints from the in-memory retriever output directly.

### 5q Configuration

Add to `MoaConfig`:

```toml
[observability.lineage]
enabled = true
channel_capacity = 8192
batch_size = 512
batch_max_age_secs = 2
journal_path = "/var/lib/moa/lineage-journal"
sample_pgvector_explain = 0.01    # 1% of pgvector queries get full EXPLAIN ANALYZE
```

When `enabled = false`, the orchestrator wires `NullLineageHandle` and the entire pipeline below the trait is dead code (one branch per turn).

### 5r Tests

In `crates/moa-lineage/sink/tests/`:

1. `roundtrip.rs` — emit one of each record kind, await flush, query back, assert equality (modulo `prev_hash` which is NULL in L01).
2. `backpressure.rs` — saturate the channel with a slow writer, assert `dropped_count` increments and the metric fires.
3. `crash_recovery.rs` — write into fjall, kill the writer (simulate crash), restart, assert pending events replay before new ones.
4. `null_sink_zero_cost.rs` — benchmark `NullSink::record` at <50 ns/call.

In `crates/moa-cli/tests/`:

5. `explain_session.rs` — seed three synthetic turns (one retrieval, one context, one generation each), run `moa explain <session>`, assert output contains the right structure.
6. `retrieve_debug.rs` — run `moa retrieve --debug` end-to-end against a test DB.

In `crates/moa-brain/tests/`:

7. `retrieval_emits_lineage.rs` — mock `LineageHandle`, run a retrieval, assert exactly one `RetrievalLineage` is emitted with the expected `top_k`.

## 6 Deliverables

- `crates/moa-lineage/{core,sink,otel}/` directories with their respective `Cargo.toml` and `src/`.
- `crates/moa-lineage/README.md`.
- `crates/moa-lineage/sink/sql/schema.sql`.
- Workspace `Cargo.toml` updated.
- `LineageHandle` trait + field added to `moa-core::ToolContext`. `NullLineageHandle` impl in `moa-core`.
- `moa-lineage-sink::MpscSink` + `NullSink` impls.
- `moa-lineage-otel::emit_*` functions.
- Orchestrator-startup wiring spawning the writer.
- Emission call sites in retriever, context compiler, all four LLM providers.
- `moa explain <session>`, `moa explain <turn>`, `moa retrieve --debug` CLI commands.
- Tests above.
- Update `architecture.md` with a new "Observability and explainability" section pointing at this pack and explaining the L01 surface.

## 7 Acceptance criteria

1. `cargo build --workspace` clean.
2. `cargo test --workspace` green.
3. `cargo clippy --workspace -- -D warnings` clean.
4. `rg "use moa_lineage" crates/` shows imports only in `moa-cli`, `moa-orchestrator*`, `moa-brain`, `moa-providers`. No imports in `moa-core` (must use the bridge trait).
5. `psql -c "SELECT * FROM analytics.turn_lineage LIMIT 1" $TEST_DB` returns the expected row shape after running an end-to-end test session.
6. `psql -c "SELECT show_chunks('analytics.turn_lineage')"` returns ≥ 1 chunk.
7. `psql -c "SELECT compression_status FROM timescaledb_information.compression_settings WHERE hypertable_name = 'turn_lineage'"` returns enabled.
8. End-to-end smoke: `moa "What's 2+2?"` against a configured DB → `moa explain <session>` shows one turn with retrieval, context, and generation records.
9. `moa retrieve --debug "test query"` returns within 5 seconds with a populated retrieval trace.
10. With `observability.lineage.enabled = false`, no rows appear in `analytics.turn_lineage` and `moa_lineage_recorded_total = 0`.
11. Backpressure test: with channel size = 4 and a slow writer, sending 1000 events results in 996 dropped and 4 captured (modulo writer drain race; assertion is `dropped + captured == 1000` and `dropped > 0`).
12. Crash-recovery test: events written to fjall before SIGKILL of the writer process arrive in TimescaleDB after restart.

## 8 Tests

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings

# Schema is created (idempotent)
psql -c "\dt analytics.turn_lineage" $TEST_DB
psql -c "\d analytics.turn_lineage" $TEST_DB

# Hypertable + compression policy active
psql -c "SELECT * FROM timescaledb_information.hypertables WHERE hypertable_name = 'turn_lineage'" $TEST_DB
psql -c "SELECT * FROM timescaledb_information.jobs WHERE proc_name LIKE 'policy_%'" $TEST_DB

# End-to-end
moa "What's 2+2?"
moa explain <session-id-from-above>
moa retrieve --debug "test"

# Disabled-cost path
MOA_OBSERVABILITY_LINEAGE_ENABLED=false moa "What's 2+2?"
psql -c "SELECT count(*) FROM analytics.turn_lineage WHERE ts > now() - interval '1 minute'" $TEST_DB
# expect: 0
```

## 9 Cleanup

- Confirm no `unwrap()` on the hot path in `MpscSink::record`. The hot path must never panic.
- Confirm `NullSink::record` benchmarks at <50 ns/call (`cargo bench`).
- Run `moa doctor` and add a `lineage` row reporting writer-worker health (last-flush-time, journal depth, dropped count).
- Verify that on graceful shutdown (`moa daemon stop`), the writer drains and prints `lineage: drained N events, 0 in journal` before exit.

## 10 What's next

**STOP.** Validate the schema in production for at least 24 hours before continuing. Issues to watch for:

- Does compression kick in on day-7 chunks? (`SELECT * FROM timescaledb_information.chunks WHERE is_compressed`)
- Does retention drop day-30 chunks? (Same view, count of chunks should stabilize.)
- What's the actual bytes-per-turn after compression? Compare to the model's 12,400 bytes raw retrieval estimate.
- Is `moa_lineage_dropped_total` ever non-zero in normal operation? If yes, raise channel capacity.

When ready, run **L02 — citation pipeline + S3 cold tier + Gemini Embedding** and **L03 — eval + Grafana dashboards** (in parallel).
