# Step L02 — Citation pipeline + S3 Parquet cold tier + Gemini Embedding

_Add `crates/moa-lineage/citation/` and `crates/moa-lineage/cold/` subcrates. Implement vendor-passthrough citation adapters for Anthropic, OpenAI, Cohere, and Vertex; add a model-agnostic cascade NLI verifier as a backstop; export aged-out hot-store data to S3 Parquet. Also: add `GeminiEmbedding2Embedder` to `crates/moa-memory/vector/` so MOA can use Google's multimodal `gemini-embedding-2` (GA April 2026) alongside `cohere-embed-v4`._

## 1 What this step is about

L01 captures retrieval, context, and generation lineage. L02 closes two gaps:

1. **Citation lineage.** The hardest production failure mode is a fluent-but-wrong answer. Vendor citation APIs (Anthropic Citations, OpenAI file_search annotations, Cohere `documents=`, Vertex grounding) give you per-claim source mappings *for free* when you give them retrieved chunks — but each speaks a different shape, and any of them can hallucinate citations. L02 normalizes them all to MOA's `Citation` shape and adds a cascade verifier (BM25 → small-NLI) as a model-agnostic backstop.

2. **Cold-tier storage.** TimescaleDB retains 30 days hot. Beyond that, data must move to S3 Parquet for cheap long-term retention. L02 adds the exporter that rolls aged chunks every 30 s / 50 MB into Hive-partitioned Parquet keyed by `workspace_id` + date.

L02 also adds **`gemini-embedding-2`** (GA April 2026; supersedes `gemini-embedding-001`) as an `Embedder` impl in `moa-memory/vector/`. This is technically orthogonal to lineage but lives in this prompt because it's a small, contained add and citation work overlaps with embedder selection. The wire format and prompt-prefix task encoding for v2 is **incompatible with v1** — adopting v2 means re-embedding all existing vectors. v1 (`gemini-embedding-001`) remains supported as a config option for workspaces that haven't migrated.

## 2 Files to read

- L01 (this prompt is built on top of it; same channel, same hypertable, same trait surface)
- `crates/moa-providers/src/{anthropic,openai,cohere,vertex}.rs` — provider response shapes
- `crates/moa-memory/vector/src/lib.rs` — `Embedder` trait + `CohereV4Embedder`
- `crates/moa-memory/graph/src/{node.rs, edge.rs}` — `ChunkId` shape, source linkage
- `crates/moa-lineage/core/src/records.rs` — `LineageEvent::Citation` placeholder to flesh out
- Anthropic Citations API docs (look up current spec at runtime)
- OpenAI file_search annotations docs
- Cohere chat with `documents=` docs
- Vertex grounding (`groundingMetadata`) docs
- Google Gemini Embedding API docs: `https://ai.google.dev/gemini-api/docs/embeddings`
  - Latest: `gemini-embedding-2` (multimodal: text/image/audio/video/PDF; 8,192 input tokens; flexible 128–3072 output dim, recommended 768/1536/3072; **no `task_type` field** — task is a prompt prefix; auto-normalizes truncated dims; default 3072)
  - Prior: `gemini-embedding-001` (text-only; 2,048 input tokens; manual normalization required for non-3072 dims; uses `taskType` field)
  - **Embedding spaces between v1 and v2 are incompatible** — switching requires re-embedding

## 3 Goal

After L02:

- `crates/moa-lineage/citation/` (package `moa-lineage-citation`) exists.
- `crates/moa-lineage/cold/` (package `moa-lineage-cold`) exists.
- `Citation` record shape is finalized in `moa-lineage-core` (replaces the L01 placeholder).
- Vendor passthrough adapters: `AnthropicCitations`, `OpenAiAnnotations`, `CohereDocuments`, `VertexGrounding`. Each implements `CitationAdapter`.
- Cascade verifier: `CascadeVerifier` chains `Bm25Stage` → `NliStage` (HHEM-2.1-open via `ort`).
- LLM provider wrappers from L01 are extended to call the appropriate adapter on each generation and emit a `LineageEvent::Citation` (alongside `Generation`).
- `crates/moa-lineage/cold/` rolls hypertable rows older than the rollover threshold to S3 Parquet via `arrow-rs` + `object_store`.
- `crates/moa-memory/vector/src/gemini.rs` exposes both `GeminiEmbedding2Embedder` (default, multimodal-capable; text-only on the existing trait) and `GeminiEmbedding1Embedder` (legacy text-only). Selection is config-driven: `embedder.name = "gemini-embedding-2"` | `"gemini-embedding-001"` | `"cohere-embed-v4"`.
- `moa lineage query` CLI command runs ad-hoc SQL against the hot tier with sensible time-window defaults.
- `moa lineage query --cold` runs the same query through DuckDB on S3 Parquet for older windows.
- `cargo build --workspace` clean. `cargo test --workspace` green.

## 4 Rules

- **Vendor responses go straight through**, normalized to one shape. No reformatting that loses information.
- **Always run the cascade verifier**, even when vendor citations are present. The verifier emits a `verified: bool` flag and `nli_score: f32` per citation. Vendor citations that fail verification are not deleted — they're flagged so the operator can see citation hallucination.
- **Cascade is two-stage and bails early.** Stage 1 (BM25) must produce ≥ 3 candidate chunks before Stage 2 (NLI) runs. If Stage 1 returns ≤ 1 candidate, skip NLI (insufficient signal). NLI is the expensive stage — don't run it speculatively.
- **NLI model is loaded once at startup**, lives in an `Arc<Session>` (ort), shared across all in-flight verification work.
- **Cold tier is one-way.** Once data is in Parquet, the hot row is deleted (TimescaleDB retention drops it). The cold tier is the system of record for >30-day data. Don't try to keep hot+cold consistent.
- **Cold tier writes happen in a separate worker**, not on the hot-path mpsc writer. Different SLOs (tens-of-MB batches, slow flush OK).
- **Gemini Embedding 2 has no `task_type` field.** Task encoding is a **prompt prefix** the embedder formats around the input (e.g., `task: search result | query: {content}` for a search query, `title: {title} | text: {content}` for a document). The embedder owns the formatting; callers do not pre-format.
- **Gemini Embedding 2 auto-normalizes truncated output dims** (768/1536). Do not double-normalize. The legacy v1 (`gemini-embedding-001`) DOES require manual L2-normalization for non-3072 dims — keep the divergence isolated to each impl.
- **Embedder selection is per-workspace**, not global. Switching a workspace's embedder triggers re-embedding (separate operational concern; out of scope for this prompt — just don't break the migration path). v1↔v2 spaces are incompatible; same applies to Cohere v3↔v4.
- **Asymmetric retrieval is encoded at embedder construction**, not at the trait level. Two embedder instances are built per workspace — one with `EmbedRole::Document` (used during ingestion), one with `EmbedRole::SearchQuery` (used during retrieval). The `Embedder` trait stays unchanged; the role lives in the impl. The `embed_as(role, text)` escape hatch on `GeminiEmbedding2Embedder` lets callers override per-call when needed (e.g., for the symmetric STS workloads in eval).
- **Gemini v2 batch ≠ a multi-part request.** A v2 `embedContent` request with N parts produces ONE aggregated embedding, NOT N independent ones. To get N separate embeddings, the embedder MUST issue N sequential single-input calls (the implementation here) or route through the Batch API. Treating `embed_batch` as "one multi-part request" is a correctness bug for retrieval/indexing workloads.

## 5 Tasks

### 5a Add the two subcrates

```sh
mkdir -p crates/moa-lineage/{citation,cold}/src
```

Add to workspace `Cargo.toml`:

```toml
"crates/moa-lineage/citation",
"crates/moa-lineage/cold",
```

Update `crates/moa-lineage/README.md` to mark the two subcrates as shipped (remove the "L02" annotation).

### 5b Finalize `Citation` records in `moa-lineage-core`

Replace the L01 placeholder `serde_json::Value` body with the real shape. In `crates/moa-lineage/core/src/records.rs`:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CitationLineage {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub ts: DateTime<Utc>,
    pub answer_text: String,
    pub answer_sentence_offsets: Vec<(u32, u32)>,  // byte offsets per sentence
    pub citations: Vec<Citation>,
    pub vendor_used: Option<String>,   // "anthropic" | "openai" | "cohere" | "vertex"
    pub verifier_used: Option<String>, // "cascade-bm25-hhem" etc
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Citation {
    /// Sentence index into `answer_sentence_offsets`.
    pub answer_span: u32,
    /// Optional sub-span (byte offsets within the sentence).
    pub answer_span_bytes: Option<(u32, u32)>,
    pub source_chunk_id: Uuid,
    pub source_node_uid: Option<Uuid>,
    /// Text the model claims to have grounded on, copied from the source.
    pub cited_text: Option<String>,
    /// Vendor-supplied score (Cohere, OpenAI). None when not provided.
    pub vendor_score: Option<f32>,
    /// Cascade verifier output. Always present after verification runs.
    pub verifier: VerifierResult,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifierResult {
    pub verified: bool,
    pub bm25_score: Option<f32>,
    pub nli_entailment: Option<f32>,
    pub nli_contradiction: Option<f32>,
    pub method: String,    // "vendor_only" | "bm25" | "bm25+nli"
}
```

Update `LineageEvent::Citation(serde_json::Value)` → `LineageEvent::Citation(CitationLineage)` and update the `record_kind()` mapping. Bump the crate's MINOR version.

### 5c `moa-lineage-citation/Cargo.toml`

```toml
[package]
name = "moa-lineage-citation"
version = "0.1.0"
edition = "2024"

[dependencies]
moa-core         = { path = "../../moa-core" }
moa-lineage-core = { path = "../core" }
moa-providers    = { path = "../../moa-providers" }
async-trait      = { workspace = true }
serde            = { workspace = true, features = ["derive"] }
serde_json       = { workspace = true }
uuid             = { workspace = true }
chrono           = { workspace = true }
tokio            = { workspace = true, features = ["sync"] }
tracing          = { workspace = true }
tantivy          = "0.22"            # BM25 stage
ort              = { version = "2", features = ["load-dynamic"] } # NLI inference
ndarray          = "0.16"
tokenizers       = "0.20"            # Tokenizer for HHEM model
thiserror        = { workspace = true }
```

### 5d Define `CitationAdapter` and `CitationVerifier` traits

`crates/moa-lineage/citation/src/lib.rs`:

```rust
mod adapters;
mod verifiers;
mod cascade;

pub use adapters::{
    CitationAdapter, AdapterError,
    AnthropicCitations, OpenAiAnnotations, CohereDocuments, VertexGrounding,
};
pub use verifiers::{
    CitationVerifier, VerificationInput, Bm25Verifier, NliVerifier,
};
pub use cascade::{CascadeVerifier, CascadeConfig};

use moa_lineage_core::{Citation, CitationLineage, VerifierResult};
```

`adapters.rs` — one trait, four impls. Each `extract_citations` takes the raw provider response and the retrieved chunk set and returns normalized `Citation`s.

```rust
#[async_trait::async_trait]
pub trait CitationAdapter: Send + Sync {
    fn provider(&self) -> &'static str;

    async fn extract_citations(
        &self,
        provider_response: &serde_json::Value,
        retrieved_chunks: &[ChunkRef],
    ) -> Result<Vec<Citation>, AdapterError>;
}

#[derive(Clone, Debug)]
pub struct ChunkRef {
    pub chunk_id: Uuid,
    pub source_node_uid: Option<Uuid>,
    pub text: String,
    pub provider_doc_id: String,  // ID the provider used in its response
}
```

Implementations:

- **`AnthropicCitations`** — extract from `content[].citations[]` blocks. Per-sentence char/page/block ranges. Map back to chunk_id via `provider_doc_id` (Anthropic uses document indexes when documents are passed inline).
  - **Note**: Anthropic Citations API is mutually exclusive with Structured Outputs. Surface this as an `AdapterError::IncompatibleMode` if both are configured.

- **`OpenAiAnnotations`** — extract from `output[].content[].annotations[]` (Responses API) or `choices[0].message.tool_calls[]` (legacy). Annotations carry `file_id` + `text` substring but no source-side offsets. To get source text, the caller must re-query `file_search_call.results` (passed into the adapter as `retrieved_chunks`).

- **`CohereDocuments`** — extract from `citations[]` in chat completion. Each citation has `start`, `end` (answer-side char offsets) and `document_ids[]`. Map document_ids to chunk_ids via the order chunks were passed.

- **`VertexGrounding`** — extract from `candidates[].groundingMetadata.{groundingChunks[], groundingSupports[]}`. Each support has `segment.{startIndex, endIndex, text}` (answer-side) and `groundingChunkIndices[]` (which chunks). Note Gemini ≥2.5 removed per-citation confidence scores — set `vendor_score: None`.

Each adapter's tests should include one canonical sample response from the provider's docs.

### 5e Implement the cascade verifier

`verifiers.rs`:

```rust
#[derive(Clone, Debug)]
pub struct VerificationInput<'a> {
    pub answer_sentence: &'a str,
    pub candidate_chunks: &'a [ChunkRef],
}

#[async_trait::async_trait]
pub trait CitationVerifier: Send + Sync {
    async fn verify(&self, input: VerificationInput<'_>) -> Vec<(Uuid, VerifierResult)>;
}

pub struct Bm25Verifier {
    /// Tantivy index built per turn from the retrieved chunks.
    /// (Re-indexing 10 chunks is fast; reuse is hard because chunks differ per turn.)
    schema: tantivy::schema::Schema,
}

pub struct NliVerifier {
    /// HHEM-2.1-open ONNX model, loaded once at startup.
    session: std::sync::Arc<ort::Session>,
    tokenizer: tokenizers::Tokenizer,
}
```

`cascade.rs`:

```rust
pub struct CascadeConfig {
    pub bm25_top_k: usize,           // default 3
    pub bm25_min_candidates: usize,  // default 1; below this, skip NLI
    pub nli_threshold: f32,          // default 0.5 entailment
    pub max_concurrent_nli: usize,   // default 4 (CPU-bound)
}

pub struct CascadeVerifier {
    bm25: Bm25Verifier,
    nli: Option<NliVerifier>,    // None if NLI is disabled by config
    config: CascadeConfig,
}

#[async_trait::async_trait]
impl CitationVerifier for CascadeVerifier {
    async fn verify(&self, input: VerificationInput<'_>) -> Vec<(Uuid, VerifierResult)> {
        // Stage 1: BM25 over candidate_chunks
        let bm25_hits = self.bm25.score(
            input.answer_sentence,
            input.candidate_chunks,
            self.config.bm25_top_k,
        ).await;

        if bm25_hits.len() < self.config.bm25_min_candidates {
            return bm25_hits.into_iter().map(|(uid, score)| (
                uid,
                VerifierResult {
                    verified: false,
                    bm25_score: Some(score),
                    nli_entailment: None,
                    nli_contradiction: None,
                    method: "bm25".into(),
                },
            )).collect();
        }

        // Stage 2: NLI on the BM25 shortlist (if configured)
        if let Some(nli) = &self.nli {
            let nli_results = nli.verify(VerificationInput {
                answer_sentence: input.answer_sentence,
                candidate_chunks: &filter_to_bm25_top(&bm25_hits, input.candidate_chunks),
            }).await;
            return merge_bm25_with_nli(bm25_hits, nli_results, self.config.nli_threshold);
        }

        // No NLI available; report BM25 only with verified=false (insufficient evidence).
        bm25_hits.into_iter().map(|(uid, score)| (
            uid,
            VerifierResult {
                verified: score > 5.0,  // tantivy BM25 floor; tune in tests
                bm25_score: Some(score),
                nli_entailment: None,
                nli_contradiction: None,
                method: "bm25".into(),
            },
        )).collect()
    }
}
```

The HHEM-2.1-open ONNX model file should ship as a separate crate asset (downloaded by `cargo xtask download-models` or fetched on first use). Don't commit binary blobs to git. Document the model's license (Apache 2.0) and provenance in the verifier's module docstring.

### 5f Wire into provider wrappers

In each LLM provider wrapper from L01, after `GenerationLineage` is emitted, also produce a `CitationLineage`:

```rust
let adapter: &dyn CitationAdapter = match provider {
    "anthropic" => &self.anthropic_citations,
    "openai"    => &self.openai_annotations,
    "cohere"    => &self.cohere_documents,
    "vertex"    => &self.vertex_grounding,
    _ => return Ok(()),  // unknown provider, skip citation step
};

let citations = adapter.extract_citations(&raw_response, retrieved_chunks).await?;
let verified = self.cascade_verifier.verify_all(
    &answer_text,
    &answer_sentence_offsets,
    &citations,
    retrieved_chunks,
).await;

let citation_event = CitationLineage {
    turn_id, session_id, workspace_id, user_id,
    ts: Utc::now(),
    answer_text: answer_text.clone(),
    answer_sentence_offsets,
    citations: verified,
    vendor_used: Some(provider.into()),
    verifier_used: Some("cascade-bm25-hhem".into()),
};

let json = serde_json::to_value(LineageEvent::Citation(citation_event))?;
ctx.lineage.record(json);
```

`verify_all` is a small fan-out helper: for each sentence in `answer_sentence_offsets`, run the cascade verifier against the chunks the vendor cited (or, if vendor cited nothing, the full retrieved set). Return the merged Citation list with `VerifierResult` populated.

### 5g `moa-lineage-cold/Cargo.toml`

```toml
[package]
name = "moa-lineage-cold"
version = "0.1.0"
edition = "2024"

[dependencies]
moa-lineage-core  = { path = "../core" }
arrow             = { version = "53", features = ["prettyprint"] }
parquet           = { version = "53", features = ["async", "arrow"] }
object_store      = { version = "0.11", features = ["aws"] }
tokio             = { workspace = true, features = ["fs", "rt", "macros", "time"] }
tokio-postgres    = { workspace = true }
deadpool-postgres = { workspace = true }
chrono            = { workspace = true }
tracing           = { workspace = true }
serde_json        = { workspace = true }
uuid              = { workspace = true }
bytes             = "1"
thiserror         = { workspace = true }
```

### 5h Cold-tier exporter

`crates/moa-lineage/cold/src/lib.rs`:

```rust
mod exporter;
mod schema;

pub use exporter::{ColdTierExporter, ColdTierConfig, ExporterStats};
pub use schema::lineage_arrow_schema;
```

`schema.rs` — define an Arrow schema matching the hypertable:

```rust
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use std::sync::Arc;

pub fn lineage_arrow_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("turn_id",        DataType::FixedSizeBinary(16), false),
        Field::new("session_id",     DataType::FixedSizeBinary(16), false),
        Field::new("user_id",        DataType::FixedSizeBinary(16), false),
        Field::new("workspace_id",   DataType::FixedSizeBinary(16), false),
        Field::new("ts",             DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())), false),
        Field::new("tier",           DataType::Int16, false),
        Field::new("record_kind",    DataType::Int16, false),
        Field::new("payload",        DataType::Utf8, false),    // canonical JSON string
        Field::new("answer_text",    DataType::Utf8, true),
        Field::new("integrity_hash", DataType::FixedSizeBinary(32), false),
        Field::new("prev_hash",      DataType::FixedSizeBinary(32), true),
    ]))
}
```

`exporter.rs` — periodic worker:

```rust
pub struct ColdTierConfig {
    pub bucket: String,                   // e.g. "moa-lineage"
    pub prefix: String,                   // e.g. "v1"
    pub roll_interval: Duration,          // 30s default
    pub roll_size_mb: u64,                // 50 MB default
    pub source_age_threshold_hours: u64,  // start exporting rows older than 23h (1h before retention drops them)
    pub partition_by_workspace: bool,     // default true
    pub zstd_level: i32,                  // 3 default (fast)
}

pub struct ColdTierExporter {
    pool: deadpool_postgres::Pool,
    store: Arc<dyn object_store::ObjectStore>,
    config: ColdTierConfig,
}

impl ColdTierExporter {
    pub async fn run(self, cancel: tokio_util::sync::CancellationToken) {
        let mut interval = tokio::time::interval(self.config.roll_interval);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick()    => {}
            }
            if let Err(e) = self.flush_one_window().await {
                tracing::error!("cold-tier flush failed: {e}");
            }
        }
    }

    async fn flush_one_window(&self) -> anyhow::Result<()> {
        // 1. SELECT rows older than threshold, batched per-workspace per-day.
        // 2. Build Arrow RecordBatches (row group ~256 K rows = ~50 MB after ZSTD).
        // 3. Write Parquet to object_store at:
        //      {prefix}/workspace_id={uuid}/dt={YYYY-MM-DD}/{ts_first}-{uuid}.parquet
        // 4. Update analytics.lineage_export_progress to record what we exported.
        // 5. We do NOT delete from hot — TimescaleDB retention does that.
        Ok(())
    }
}
```

Add a small bookkeeping table to track export progress:

```sql
CREATE TABLE IF NOT EXISTS analytics.lineage_export_progress (
    workspace_id UUID NOT NULL,
    day          DATE NOT NULL,
    last_ts      TIMESTAMPTZ NOT NULL,
    rows_exported BIGINT NOT NULL,
    parquet_uri  TEXT NOT NULL,
    PRIMARY KEY (workspace_id, day)
);
```

Spawn the exporter from the orchestrator startup, sibling to the L01 writer:

```rust
let cold_handle = ColdTierExporter::new(pool.clone(), store, cfg).run(shutdown_token);
self.lineage_cold_handles.push(cold_handle);
```

### 5i Add Gemini Embedding (`gemini-embedding-2`, with `gemini-embedding-001` as a legacy option)

Three deltas from `gemini-embedding-001` to plan around:

1. **No `task_type` field on the wire.** Task is encoded as a **prompt prefix** the embedder formats around the input. Asymmetric retrieval uses `task: search result | query: {content}` (query side) and `title: {title} | text: {content}` (document side, with `title: none` if absent). Symmetric tasks use `task: classification | query: {content}` etc.
2. **Auto-normalization of truncated dims** — server-side. v1 needs manual L2 normalization for non-3072 dims; v2 does not.
3. **Aggregation semantics** — multiple parts in one content entry produce ONE aggregated multimodal embedding (text + image fused). Multiple content entries → multiple embeddings. For high-throughput batching, use the **Gemini Batch API** (50% discount, async); the inline `embedContent` is one-at-a-time.

Auth in both: `x-goog-api-key: $GEMINI_API_KEY` HTTP header (do **not** pass via query parameter).

`crates/moa-memory/vector/src/gemini.rs`:

```rust
//! Gemini Embedding clients.
//!
//! - `GeminiEmbedding2Embedder` — gemini-embedding-2 (GA April 2026). Multimodal in API
//!   (text/image/audio/video/PDF) but exposed here as text-only via the existing
//!   `Embedder` trait. Multimodal embedding is a future capability (gated behind a
//!   separate `MultimodalEmbedder` trait when MOA's chunking story extends beyond text).
//! - `GeminiEmbedding1Embedder` — gemini-embedding-001 (legacy text-only). Kept for
//!   workspaces that haven't migrated; v1↔v2 embedding spaces are incompatible.

use crate::{Embedder, EmbedderError, EmbeddingVector};
use async_trait::async_trait;
use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, Serialize};

const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";

// ─────────────────────────── Role / prompt-prefix encoding ────────────────────────────

/// Task encoding for `gemini-embedding-2`. The model has no server-side `taskType`
/// field; instead, the text is wrapped with a prefix that signals the task to the
/// embedder. Documents and queries get different prefixes for asymmetric retrieval.
#[derive(Clone, Debug)]
pub enum EmbedRole {
    /// Document side (asymmetric retrieval). Wraps as `title: {title} | text: {content}`.
    /// If `title` is None, uses `title: none`.
    Document { title: Option<String> },

    /// Query side, generic search. Wraps as `task: search result | query: {content}`.
    SearchQuery,

    /// Query side, question-answering. Wraps as `task: question answering | query: {content}`.
    QuestionAnsweringQuery,

    /// Query side, fact-checking. Wraps as `task: fact checking | query: {content}`.
    FactCheckingQuery,

    /// Query side, code retrieval. Wraps as `task: code retrieval | query: {content}`.
    CodeRetrievalQuery,

    /// Symmetric — classification. Wraps as `task: classification | query: {content}`.
    Classification,

    /// Symmetric — clustering. Wraps as `task: clustering | query: {content}`.
    Clustering,

    /// Symmetric — sentence similarity. Wraps as `task: sentence similarity | query: {content}`.
    /// Per the docs, do NOT use this for retrieval — it's strictly for STS-style scoring.
    SentenceSimilarity,

    /// Caller has already formatted the prefix; pass through as-is.
    Raw,
}

impl EmbedRole {
    fn format(&self, content: &str) -> String {
        match self {
            EmbedRole::Document { title } => {
                let t = title.as_deref().unwrap_or("none");
                format!("title: {t} | text: {content}")
            }
            EmbedRole::SearchQuery            => format!("task: search result | query: {content}"),
            EmbedRole::QuestionAnsweringQuery => format!("task: question answering | query: {content}"),
            EmbedRole::FactCheckingQuery      => format!("task: fact checking | query: {content}"),
            EmbedRole::CodeRetrievalQuery     => format!("task: code retrieval | query: {content}"),
            EmbedRole::Classification         => format!("task: classification | query: {content}"),
            EmbedRole::Clustering             => format!("task: clustering | query: {content}"),
            EmbedRole::SentenceSimilarity     => format!("task: sentence similarity | query: {content}"),
            EmbedRole::Raw                    => content.to_owned(),
        }
    }
}

// ───────────────────────────── gemini-embedding-2 ─────────────────────────────────────

/// gemini-embedding-2 (GA April 2026). Multimodal capable; this impl is text-only.
/// Server-side auto-normalization of truncated dims (768/1536) — do not double-normalize.
pub struct GeminiEmbedding2Embedder {
    client: Client,
    headers: HeaderMap,
    output_dim: u16,        // 128..=3072; recommended 768/1536/3072. Default 3072.
    default_role: EmbedRole,
}

#[derive(Serialize)]
struct V2Request {
    content: V2Content,
    /// Per the v2 REST example in https://ai.google.dev/gemini-api/docs/embeddings,
    /// this field is snake_case on the wire (`output_dimensionality`), unlike v1's
    /// camelCase `taskType` / `outputDimensionality`. Google's Gen AI REST is
    /// inconsistent across model versions — match each model's documented spelling.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_dimensionality: Option<u16>,
}

#[derive(Serialize)]
struct V2Content { parts: Vec<V2Part> }

#[derive(Serialize)]
struct V2Part { text: String }

#[derive(Deserialize)]
struct V2Response {
    embedding: V2Values,
}

#[derive(Deserialize)]
struct V2Values { values: Vec<f32> }

impl GeminiEmbedding2Embedder {
    pub fn new(
        api_key: String,
        output_dim: u16,
        default_role: EmbedRole,
    ) -> Result<Self, EmbedderError> {
        if !(128..=3072).contains(&output_dim) {
            return Err(EmbedderError::Config(format!(
                "gemini-embedding-2 output_dim must be in 128..=3072, got {output_dim}"
            )));
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-goog-api-key", api_key.parse()
            .map_err(|e| EmbedderError::Config(format!("invalid api key header: {e}")))?);
        headers.insert("content-type", "application/json".parse().unwrap());
        Ok(Self {
            client: Client::new(),
            headers,
            output_dim,
            default_role,
        })
    }

    /// Override the role for a single call (e.g., embed a query when the embedder was
    /// constructed in document mode for ingestion).
    pub async fn embed_as(
        &self,
        role: &EmbedRole,
        text: &str,
    ) -> Result<EmbeddingVector, EmbedderError> {
        let formatted = role.format(text);
        let url = format!("{ENDPOINT}/models/gemini-embedding-2:embedContent");
        let body = V2Request {
            content: V2Content { parts: vec![V2Part { text: formatted }] },
            output_dimensionality: Some(self.output_dim),
        };
        let resp: V2Response = self.client
            .post(&url)
            .headers(self.headers.clone())
            .json(&body)
            .send().await?
            .error_for_status()?
            .json().await?;
        Ok(EmbeddingVector(resp.embedding.values))
    }
}

#[async_trait]
impl Embedder for GeminiEmbedding2Embedder {
    fn name(&self) -> &str { "gemini-embedding-2" }
    fn output_dim(&self) -> u16 { self.output_dim }

    async fn embed(&self, text: &str) -> Result<EmbeddingVector, EmbedderError> {
        // Already auto-normalized server-side for truncated dims; do NOT renormalize.
        self.embed_as(&self.default_role, text).await
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<EmbeddingVector>, EmbedderError> {
        // gemini-embedding-2 aggregates parts within ONE content entry into a single
        // embedding — that is NOT batch behavior. For real high-throughput batching,
        // use the Gemini Batch API (async, 50% discount). This sequential fallback is
        // a correctness baseline; replace with the Batch API integration when
        // ingestion-side throughput becomes a bottleneck (separate prompt).
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
}

// ───────────────────────────── gemini-embedding-001 (legacy) ──────────────────────────

/// gemini-embedding-001 — text-only, taskType-based, manual normalization for non-3072 dims.
pub struct GeminiEmbedding1Embedder {
    client: Client,
    headers: HeaderMap,
    output_dim: u16,
    task_type: V1TaskType,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum V1TaskType {
    SemanticSimilarity,
    Classification,
    Clustering,
    RetrievalDocument,
    RetrievalQuery,
    CodeRetrievalQuery,
    QuestionAnswering,
    FactVerification,
}

#[derive(Serialize)]
struct V1Request {
    #[serde(rename = "taskType")]
    task_type: V1TaskType,
    content: V2Content,
    #[serde(rename = "outputDimensionality", skip_serializing_if = "Option::is_none")]
    output_dimensionality: Option<u16>,
}

impl GeminiEmbedding1Embedder {
    pub fn new(api_key: String, output_dim: u16, task_type: V1TaskType)
        -> Result<Self, EmbedderError>
    {
        if !(128..=3072).contains(&output_dim) {
            return Err(EmbedderError::Config(format!(
                "gemini-embedding-001 output_dim must be in 128..=3072, got {output_dim}"
            )));
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-goog-api-key", api_key.parse()
            .map_err(|e| EmbedderError::Config(format!("invalid api key header: {e}")))?);
        headers.insert("content-type", "application/json".parse().unwrap());
        Ok(Self { client: Client::new(), headers, output_dim, task_type })
    }
}

#[async_trait]
impl Embedder for GeminiEmbedding1Embedder {
    fn name(&self) -> &str { "gemini-embedding-001" }
    fn output_dim(&self) -> u16 { self.output_dim }

    async fn embed(&self, text: &str) -> Result<EmbeddingVector, EmbedderError> {
        let url = format!("{ENDPOINT}/models/gemini-embedding-001:embedContent");
        let body = V1Request {
            task_type: self.task_type.clone(),
            content: V2Content { parts: vec![V2Part { text: text.to_owned() }] },
            output_dimensionality: Some(self.output_dim),
        };
        let resp: V2Response = self.client
            .post(&url)
            .headers(self.headers.clone())
            .json(&body)
            .send().await?
            .error_for_status()?
            .json().await?;

        // v1 requires MANUAL L2 normalization for non-3072 dims.
        let mut values = resp.embedding.values;
        if self.output_dim != 3072 {
            let norm = values.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for v in &mut values { *v /= norm; }
            }
        }
        Ok(EmbeddingVector(values))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<EmbeddingVector>, EmbedderError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts { out.push(self.embed(t).await?); }
        Ok(out)
    }
}
```

Update `crates/moa-memory/vector/src/lib.rs` exports:

```rust
mod cohere;
mod gemini;
mod pgvector;
mod turbopuffer;

pub use cohere::CohereV4Embedder;
pub use gemini::{GeminiEmbedding2Embedder, GeminiEmbedding1Embedder, EmbedRole, V1TaskType};
pub use pgvector::PgvectorStore;
pub use turbopuffer::TurbopufferStore;
```

Add to config:

```toml
[memory.vector.embedder]
# One of: "gemini-embedding-2" | "gemini-embedding-001" | "cohere-embed-v4"
name = "gemini-embedding-2"
output_dim = 1536    # 128..=3072 for Gemini; recommended 768 / 1536 / 3072

[memory.vector.embedder.gemini]
api_key_env = "GEMINI_API_KEY"
# v2-only: which prefix to apply by default. Constructed instances are role-pinned;
# the retriever can override per call via `embed_as`. Choices match EmbedRole variants.
default_role = "search_query"          # "search_query" for retriever-side embedders
                                       # "document"     for ingestion-side embedders
                                       # "question_answering" / "fact_checking" / "code_retrieval"
                                       # "classification" / "clustering" / "sentence_similarity"
                                       # "raw" if pre-formatted upstream

# v1-only: which taskType to send. Ignored by v2.
task_type = "RETRIEVAL_DOCUMENT"       # one of the V1TaskType variants
```

In the embedder factory function (wherever `CohereV4Embedder` is currently constructed), dispatch by name. Note the factory is typically called twice — once for ingestion (Document role) and once for retrieval (SearchQuery role) — so the role parameter is plumbed in by the caller, not the config:

```rust
fn build_embedder(
    config: &MoaConfig,
    role: EmbedderConstructionRole,   // Ingestion | Retrieval
) -> anyhow::Result<Arc<dyn Embedder>> {
    let cfg = &config.memory.vector.embedder;
    match cfg.name.as_str() {
        "cohere-embed-v4" => Ok(Arc::new(CohereV4Embedder::new(
            std::env::var(&cfg.cohere.api_key_env)?,
            cfg.output_dim,
            role.into_cohere_input_type(),
        )?)),
        "gemini-embedding-2" => {
            let default_role = match role {
                EmbedderConstructionRole::Ingestion =>
                    EmbedRole::Document { title: None },
                EmbedderConstructionRole::Retrieval =>
                    cfg.gemini.default_role.parse_or(EmbedRole::SearchQuery),
            };
            Ok(Arc::new(GeminiEmbedding2Embedder::new(
                std::env::var(&cfg.gemini.api_key_env)?,
                cfg.output_dim,
                default_role,
            )?))
        }
        "gemini-embedding-001" => {
            let task_type = match role {
                EmbedderConstructionRole::Ingestion => V1TaskType::RetrievalDocument,
                EmbedderConstructionRole::Retrieval => cfg.gemini.task_type.clone(),
            };
            Ok(Arc::new(GeminiEmbedding1Embedder::new(
                std::env::var(&cfg.gemini.api_key_env)?,
                cfg.output_dim,
                task_type,
            )?))
        }
        other => anyhow::bail!("unknown embedder: {other}"),
    }
}
```

Document in `architecture.md`:

- Switching embedders requires re-embedding existing vectors. Cohere v4 (1024-dim Matryoshka), Gemini v1 (128–3072), and Gemini v2 (128–3072) are all incompatible with each other at the index level.
- **`gemini-embedding-2` input limit is 8,192 tokens** (vs 2,048 for v1). Consider raising MOA's default chunk size for v2-targeted ingestion if you are currently chunking at 2,048-token boundaries to fit v1.
- v2 does not support a `task_type` parameter; the embedder formats prompt prefixes per `EmbedRole`. Production ingestion runs with `Document`, retrieval runs with `SearchQuery` (or one of the more specific query-side variants). Symmetric workloads (classification, clustering, STS) are out-of-band utilities, not part of the retrieval hot path.
- v2 multimodal embedding (image/audio/video/PDF) is **not wired** in this prompt. The current `Embedder` trait is text-in/vector-out. Adding multimodal requires (a) extending MOA's chunker to emit non-text chunks, (b) a separate `MultimodalEmbedder` trait, and (c) a sandboxing/file-handling story for binary inputs. Track as a separate prompt when MOA's ingestion is ready for it.

### 5j Add `moa lineage query` CLI

Hot tier: SQL passthrough with safety rails (read-only, time-bounded, workspace-scoped).

```rust
async fn lineage_query(
    config: &MoaConfig,
    sql: &str,
    cold: bool,
    since: chrono::Duration,
) -> Result<String> {
    if !is_select_only(sql) {
        bail!("only SELECT queries permitted");
    }
    if cold {
        // DuckDB on S3 Parquet
        let duckdb = duckdb::Connection::open_in_memory()?;
        duckdb.execute_batch(&format!(
            "INSTALL httpfs; LOAD httpfs; \
             SET s3_region='{}'; SET s3_access_key_id='{}'; SET s3_secret_access_key='{}';",
            config.cold.region, config.cold.access_key_id, config.cold.secret_access_key
        ))?;
        let parquet_glob = format!(
            "s3://{}/{}/*/dt=*/*.parquet",
            config.cold.bucket, config.cold.prefix
        );
        let prepared = sql.replace("FROM lineage", &format!("FROM read_parquet('{}')", parquet_glob));
        run_duckdb_query(&duckdb, &prepared)
    } else {
        let pool = config.lineage_pool().await?;
        let conn = pool.get().await?;
        let scoped = format!(
            "SELECT * FROM ({}) sub WHERE ts > now() - $1 LIMIT 10000",
            sql.replace("FROM lineage", "FROM analytics.turn_lineage")
        );
        let rows = conn.query(&scoped, &[&since]).await?;
        Ok(format_rows_as_table(&rows))
    }
}
```

Add `moa lineage query` to the CLI subcommand table. Document common queries in `architecture.md`:

```
moa lineage query "SELECT count(*) FROM lineage WHERE record_kind = 1 AND jsonb_array_length(payload #> '{retrieval,top_k}') = 0"
moa lineage query "SELECT workspace_id, count(*) FROM lineage WHERE record_kind = 4 AND payload #> '{citations,0,verifier,verified}' = 'false' GROUP BY workspace_id" --since=24h
moa lineage query --cold "..." --since=90d
```

### 5k Tests

In `crates/moa-lineage/citation/tests/`:

1. `anthropic_adapter.rs` — feed a canonical Anthropic Citations response (from their docs), assert the adapter produces the expected `Citation`s.
2. `openai_adapter.rs` — same with OpenAI Responses API annotations.
3. `cohere_adapter.rs` — same with Cohere `documents=` chat.
4. `vertex_adapter.rs` — same with Vertex `groundingMetadata`.
5. `cascade_bm25_only.rs` — verifier without NLI configured returns `verified` based on BM25 floor.
6. `cascade_bm25_nli.rs` — full cascade with HHEM-2.1-open returns `verified` only when entailment ≥ threshold. Fixture: known entailing pair, known non-entailing pair, known contradiction pair.
7. `vendor_hallucinated_citation.rs` — Anthropic adapter returns a citation the verifier marks unverified; assert both the original citation AND the failed verification appear in the output (we don't filter, we flag).
8. `incompatible_modes.rs` — `AnthropicCitations` returns `IncompatibleMode` error when the request has structured outputs enabled.

In `crates/moa-lineage/cold/tests/`:

9. `parquet_roundtrip.rs` — write a known set of rows to Parquet, read them back via DuckDB, assert equality.
10. `partition_layout.rs` — assert keys land at the expected `workspace_id={uuid}/dt={date}/...` paths.

In `crates/moa-memory/vector/tests/`:

11. `gemini_v2_smoke.rs` — calls `gemini-embedding-2` against a recorded fixture (use `mockito` or `wiremock` to avoid live API in CI), asserts the request body has no `taskType` field, the URL is `gemini-embedding-2:embedContent`, the auth header is `x-goog-api-key`, and the returned vector has the configured `output_dim`.
12. `gemini_v2_role_prefixes.rs` — for each `EmbedRole` variant, assert the formatted body's `parts[0].text` starts with the documented prefix (e.g. `task: search result | query: ` for `SearchQuery`, `title: none | text: ` for a default `Document`).
13. `gemini_v2_no_double_normalize.rs` — given a server response that's already unit-norm at 768 dims, assert the embedder does NOT renormalize (output equals input bit-for-bit).
14. `gemini_v1_legacy.rs` — calls `gemini-embedding-001` with `RetrievalDocument` task type, asserts the request body has the `taskType` field, asserts manual L2 normalization is applied for non-3072 dims (output norm ≈ 1.0).
15. `embedder_factory.rs` — config selects correctly between Cohere v4, Gemini v2, and Gemini v1; ingestion-side and retrieval-side construction produces the right roles/task_types.

In `crates/moa-cli/tests/`:

16. `lineage_query.rs` — end-to-end on hot tier.
17. `lineage_query_cold.rs` — end-to-end on Parquet via DuckDB (use `tempdir` + local `object_store::local::LocalFileSystem` to avoid live S3 in CI).

## 6 Deliverables

- `crates/moa-lineage/{citation,cold}/` directories with `Cargo.toml` + `src/`.
- `Citation` records finalized in `moa-lineage-core`.
- Four citation adapters + cascade verifier + fan-out helper.
- LLM provider wrappers extended to emit `LineageEvent::Citation`.
- Cold-tier exporter spawned from orchestrator startup; bookkeeping table created.
- `GeminiEmbedding2Embedder` (default, with `EmbedRole` task-prefix encoding) and `GeminiEmbedding1Embedder` (legacy `taskType`-based) impls, factory wiring with per-construction-role dispatch, config keys.
- `moa lineage query` CLI (hot + `--cold`).
- Tests above.
- `architecture.md` updated.
- ONNX model file + license noted (HHEM-2.1-open Apache 2.0).

## 7 Acceptance criteria

1. `cargo build --workspace` clean.
2. `cargo test --workspace` green (CI tests use mocked vendors and local FS for cold tier).
3. End-to-end smoke: `moa "What is OAuth?"` against a configured DB with Anthropic provider → `moa explain <session>` shows a turn with retrieval, context, generation, AND citation records, all bound to the same `turn_id`.
4. `moa lineage query "SELECT record_kind, count(*) FROM lineage GROUP BY record_kind"` shows non-zero counts for kinds 1, 2, 3, 4.
5. After waiting >23 hours (or with a test config that drops `source_age_threshold_hours` to 0), `s3://moa-lineage/v1/workspace_id=*/dt=*/*.parquet` contains at least one file.
6. `analytics.lineage_export_progress` has rows tracking what's been exported.
7. `moa lineage query --cold "SELECT count(*) FROM lineage" --since=30d` returns the expected count.
8. With `embedder.name = "gemini-embedding-2"` and `output_dim = 1536`, ingesting one document and retrieving against it works end-to-end. Inspecting the outbound HTTP request in a recorded fixture shows: URL `…/models/gemini-embedding-2:embedContent`, header `x-goog-api-key: …`, body `{ "content": { "parts": [{ "text": "title: none | text: …" }] }, "output_dimensionality": 1536 }` (snake_case, per Google's v2 REST docs), and NO `taskType` or `model` field in the body. The retrieval-side embedder produces the `task: search result | query: …` prefix instead.
9. With `embedder.name = "gemini-embedding-001"` and `output_dim = 1536`, the request body contains `taskType` (e.g. `RETRIEVAL_DOCUMENT`) and the returned vector is L2-normalized (norm ≈ 1.0) on the client.
10. `rg "FileMemoryStore" crates/` still returns 0 hits (no regression on the cutover).
11. The cascade verifier emits `verifier_used: "cascade-bm25-hhem"` on every citation when the NLI model is configured; emits `"vendor_only"` only if NLI is explicitly disabled.

## 8 Tests

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings

# End-to-end with all providers (requires API keys):
ANTHROPIC_API_KEY=... moa "What is OAuth?"
OPENAI_API_KEY=...    moa --provider=openai "What is OAuth?"
COHERE_API_KEY=...    moa --provider=cohere "What is OAuth?"
GEMINI_API_KEY=...    moa --provider=vertex "What is OAuth?"

# Verify all four citation paths populate
moa lineage query "SELECT payload #>> '{vendor_used}' AS vendor, count(*) FROM lineage \
                   WHERE record_kind = 4 GROUP BY 1"

# Cold tier
moa lineage query --cold "SELECT count(*) FROM lineage" --since=90d

# Gemini embedder (v2 default; v1 legacy)
GEMINI_API_KEY=... moa memory ingest /path/to/test.md
moa memory search "test query"

# Verify the v2 wire shape against a recorded fixture (no live API)
cargo test -p moa-memory-vector --test gemini_v2_smoke
cargo test -p moa-memory-vector --test gemini_v2_role_prefixes
cargo test -p moa-memory-vector --test gemini_v1_legacy
```

## 9 Cleanup

- Confirm no large model binaries committed to git. Add `*.onnx` to `.gitignore`.
- Confirm cold-tier worker survives `pg_terminate_backend` on a stuck connection (tests its retry path).
- `cargo bench citation_cascade` — verifier overhead on a 5-citation answer should be <50 ms (BM25 only) or <250 ms (with NLI on CPU).

## 10 What's next

**L03** — Eval harness wiring + Grafana dashboards + alerts (can run in parallel with L02; if you've finished both, run L04 — compliance audit tier — last).
