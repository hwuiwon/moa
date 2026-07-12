//! Tenant knowledge ingestion pipeline from provider records to graph/vector writes.

mod graph_writer;
mod materialization;
mod page;
mod record;
mod steps;

pub use graph_writer::{GraphWriteReport, KnowledgeGraphWriter, MemoryKnowledgeGraphWriter};
pub use record::parse_input_from_record;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use futures_util::{StreamExt, TryStreamExt, stream};
use moa_core::traits::EmbeddingProvider;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
};
use moa_memory_types::MemoryScope;
use serde_json::{Value, json};
use tracing::{Instrument, Span};
use uuid::Uuid;

use crate::{
    chunking::{ChunkingConfig, blocks_to_chunks, content_hash, elements_to_blocks},
    contact_groups::derive_contact_groups_from_object,
    domain::{
        DocumentVersion, FetchedRecordContent, IngestionStepStatus, KnowledgeChunk,
        KnowledgeObject, KnowledgeSyncCounters, ParseInput, ParsedDocument, ProviderRecord,
        RecordPage, SyncRunStatus,
    },
    error::{Error, Result},
    graph_delta::{
        GraphEdgeUpsert, KnowledgeGraphDelta, document_chunk_delta_with_semantics,
        semantic_chunk_link_count, stable_uid,
    },
    normalize::{normalize_text, redact_provider_metadata},
    observability::{
        FailureClassification, StepLabels, StepOutcome, build_step_row, classify_failure,
        failed_outcome, record_step_observability,
    },
    parser::DocumentParser,
    providers::RecordContentFetcher,
    repository::{DocumentVersionIngestionClaim, KnowledgeRepository},
    semantic_graph::{
        SEMANTIC_GRAPH_MODEL, SEMANTIC_GRAPH_PROMPT_VERSION, SEMANTIC_GRAPH_SCHEMA_VERSION,
        SemanticGraphExtraction, extract_chunk_semantics,
    },
};

/// Maximum objects fetched and tombstoned per source-selection prune page.
const PRUNE_BATCH_SIZE: i64 = 500;

/// Maximum number of provider records (or prune targets) processed concurrently
/// within one page/batch. Record-level version claims are the idempotency
/// boundary, so distinct objects are safe to process in parallel; this small
/// fixed cap bounds shared connection-pool and embedding-provider load.
const MAX_CONCURRENT_PAGE_RECORDS: usize = 4;

/// Summary returned after ingesting one provider page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageIngestionReport {
    /// Number of records listed by the provider page.
    pub records_listed: u64,
    /// Number of changed records ingested.
    pub records_ingested: u64,
    /// Number of records skipped as unchanged.
    pub records_skipped: u64,
    /// Number of provider-deleted records handled.
    pub records_deleted: u64,
    /// Number of new embeddings created.
    pub embeddings_created: u64,
}

/// Dependency-injected ingestion service free of Restate service types.
pub struct KnowledgeIngestionPipeline<R, P, E, G> {
    repository: Arc<R>,
    parser: Arc<P>,
    embedder: Arc<E>,
    graph: Arc<G>,
    chunking: ChunkingConfig,
    provider: String,
    parser_label: String,
    semantic_generic_entities: bool,
    content_fetcher: Option<Arc<dyn RecordContentFetcher>>,
}

/// Static pipeline settings used for chunking and observability labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeIngestionPipelineConfig {
    /// Chunking thresholds for normalized knowledge blocks.
    pub chunking: ChunkingConfig,
    /// Low-cardinality provider label for steps, spans, and metrics.
    pub provider: String,
    /// Low-cardinality parser label for steps, spans, and metrics.
    pub parser_label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordIngestionOutcome {
    Ingested { embeddings_created: u64 },
    Skipped,
}

/// One record's contribution to a page report, collected from concurrent record
/// processing and folded into `PageIngestionReport` deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageRecordContribution {
    Deleted(u64),
    Ingested { embeddings_created: u64 },
    Skipped,
}

#[derive(Debug, Clone, PartialEq)]
struct PersistedIngestion {
    delta: KnowledgeGraphDelta,
    embeddings_created: u64,
    ingested: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct SemanticGraphExtractionReport {
    extractions: Vec<SemanticGraphExtraction>,
    cache_hits: u64,
    cache_misses: u64,
    entities_extracted: u64,
    relations_extracted: u64,
    semantic_chunk_links: u64,
}
