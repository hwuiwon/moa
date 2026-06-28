//! Live extraction fixture recording for memory retrieval eval corpora.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::config::MemoryExtractionConfig;
use moa_memory_graph::NodeIndexRow;
use moa_memory_ingest::{
    EXTRACTION_PROMPT_VERSION, EntityMergeFixtureRecord, EntityMergeVerifier,
    ExtractionFixtureRecord, FactExtractor, LlmEntityMergeVerifier, LlmFactExtractor,
    MERGE_PROMPT_VERSION, RecordedFact, RecordedFactExtractor, chunk_hash, chunk_turn,
    merge_fixture_key,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

use super::runner::{
    IsolatedEvalStore, LoadedMemoryEvalCorpus, cached_embedding_provider_for_corpus,
    cleanup_eval_graph_rows,
};
use super::{
    read_ledger_jsonl, read_manifest_json, read_probes_jsonl, read_sessions_jsonl, validate_corpus,
};
use crate::kernel::FixtureStore;
use moa_eval_core::{EvalError, Result};

const CHUNK_TARGET_TOKENS: usize = 700;
const CHUNK_OVERLAP_TOKENS: usize = 100;

fn load_api_key(api_key_env: &str) -> Result<String> {
    let api_key = env::var(api_key_env).map_err(|_| {
        EvalError::InvalidConfig(format!("live recording requires {api_key_env} to be set"))
    })?;
    if api_key.trim().is_empty() {
        return Err(EvalError::InvalidConfig(format!(
            "live recording requires {api_key_env} to be non-empty"
        )));
    }
    Ok(api_key)
}

/// Options for recording live extraction fixtures for one corpus.
#[derive(Debug, Clone)]
pub struct MemoryExtractionRecordingOptions {
    corpus_dir: PathBuf,
    output_path: Option<PathBuf>,
    extraction_config: MemoryExtractionConfig,
    api_key_env: String,
    request_delay_ms: u64,
}

/// Options for recording live entity-merge fixtures for one corpus.
#[derive(Debug, Clone)]
pub struct MemoryMergeRecordingOptions {
    corpus_dir: PathBuf,
    output_path: Option<PathBuf>,
    extraction_path: Option<PathBuf>,
    extraction_config: MemoryExtractionConfig,
    api_key_env: String,
}

impl MemoryMergeRecordingOptions {
    /// Creates recording options for a corpus directory.
    #[must_use]
    pub fn new(corpus_dir: impl Into<PathBuf>) -> Self {
        Self {
            corpus_dir: corpus_dir.into(),
            output_path: None,
            extraction_path: None,
            extraction_config: MemoryExtractionConfig::default(),
            api_key_env: "MOA_COHERE_API_KEY".to_string(),
        }
    }

    /// Overrides the output merge-fixture path.
    #[must_use]
    pub fn with_output_path(mut self, output_path: impl Into<PathBuf>) -> Self {
        self.output_path = Some(output_path.into());
        self
    }

    /// Overrides the recorded extraction fixture path used by the ingestion replay.
    #[must_use]
    pub fn with_extractions_path(mut self, extraction_path: impl Into<PathBuf>) -> Self {
        self.extraction_path = Some(extraction_path.into());
        self
    }

    /// Overrides the Cohere API-key environment variable used for live merge verification.
    #[must_use]
    pub fn with_api_key_env(mut self, api_key_env: impl Into<String>) -> Self {
        self.api_key_env = api_key_env.into();
        self
    }

    /// Overrides the Cohere chat model used for live merge verification.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.extraction_config.model = model.into();
        self
    }

    /// Overrides the live merge verifier timeout in milliseconds.
    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.extraction_config.timeout_ms = timeout_ms;
        self
    }
}

impl MemoryExtractionRecordingOptions {
    /// Creates recording options for a corpus directory.
    #[must_use]
    pub fn new(corpus_dir: impl Into<PathBuf>) -> Self {
        Self {
            corpus_dir: corpus_dir.into(),
            output_path: None,
            extraction_config: MemoryExtractionConfig::default(),
            api_key_env: "MOA_COHERE_API_KEY".to_string(),
            request_delay_ms: 0,
        }
    }

    /// Overrides the output fixture path.
    #[must_use]
    pub fn with_output_path(mut self, output_path: impl Into<PathBuf>) -> Self {
        self.output_path = Some(output_path.into());
        self
    }

    /// Overrides the Cohere API-key environment variable.
    #[must_use]
    pub fn with_api_key_env(mut self, api_key_env: impl Into<String>) -> Self {
        self.api_key_env = api_key_env.into();
        self
    }

    /// Overrides the Cohere chat model.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.extraction_config.model = model.into();
        self
    }

    /// Overrides the maximum facts accepted from one chunk.
    #[must_use]
    pub fn with_max_facts_per_chunk(mut self, max_facts_per_chunk: usize) -> Self {
        self.extraction_config.max_facts_per_chunk = max_facts_per_chunk;
        self
    }

    /// Overrides the LLM request timeout in milliseconds.
    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.extraction_config.timeout_ms = timeout_ms;
        self
    }

    /// Adds a delay after each live extraction request.
    #[must_use]
    pub fn with_request_delay_ms(mut self, request_delay_ms: u64) -> Self {
        self.request_delay_ms = request_delay_ms;
        self
    }
}

/// Summary returned after recording extraction fixtures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryExtractionRecordingReport {
    /// Fixture file written by the recording run.
    pub output_path: PathBuf,
    /// Number of synthetic sessions loaded from the corpus.
    pub sessions: usize,
    /// Number of chunks recorded.
    pub chunks: usize,
    /// Number of extracted facts recorded.
    pub facts: usize,
    /// Approximate input tokens estimated from chunk text.
    pub estimated_input_tokens: usize,
    /// Approximate output tokens estimated from recorded fact summaries.
    pub estimated_output_tokens: usize,
    /// Very rough cost estimate for operator visibility.
    pub estimated_cost_usd: f64,
}

/// Summary returned after recording entity-merge fixtures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryMergeRecordingReport {
    /// Fixture file written by the recording run.
    pub output_path: PathBuf,
    /// Number of synthetic sessions loaded from the corpus.
    pub sessions: usize,
    /// Number of merge verifier decisions recorded.
    pub decisions: usize,
    /// Approximate input tokens estimated from verifier prompts.
    pub estimated_input_tokens: usize,
    /// Approximate output tokens estimated from recorded yes/no responses.
    pub estimated_output_tokens: usize,
    /// Very rough cost estimate for operator visibility.
    pub estimated_cost_usd: f64,
}

/// Records live LLM extraction fixtures for one memory-eval corpus.
pub async fn record_memory_extractions(
    options: MemoryExtractionRecordingOptions,
) -> Result<MemoryExtractionRecordingReport> {
    let manifest = read_manifest_json(&options.corpus_dir.join("manifest.json")).await?;
    let ledger = read_ledger_jsonl(&options.corpus_dir.join("ledger.jsonl")).await?;
    let sessions = read_sessions_jsonl(&options.corpus_dir.join("sessions.jsonl")).await?;
    let probes = read_probes_jsonl(&options.corpus_dir.join("probes.jsonl"), &ledger).await?;
    validate_corpus(&manifest, &ledger, &sessions, &probes)?;

    let output_path = options
        .output_path
        .unwrap_or_else(|| default_extractions_path(&manifest.corpus_id));
    let mut extraction_config = options.extraction_config.clone();
    extraction_config.api_key = load_api_key(&options.api_key_env)?;
    let extractor = LlmFactExtractor::from_config(&extraction_config).map_err(|error| {
        EvalError::InvalidConfig(format!("failed to initialize LLM fact extractor: {error}"))
    })?;
    let facts = super::gold::facts_by_id(&ledger)?;
    let mut records = existing_records(&output_path)?;
    let mut estimated_input_tokens = 0_usize;
    let mut estimated_output_tokens = 0_usize;

    for session in &sessions {
        for turn in &session.turns {
            let source = super::gold::FactSource { session, turn };
            let session_turn = super::gold::session_turn(&source, &facts)?;
            let chunks = chunk_turn(&session_turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS)
                .map_err(|error| {
                    EvalError::InvalidConfig(format!(
                        "failed to chunk synthetic session {} turn {}: {error}",
                        session.session_id, turn.turn_seq
                    ))
                })?;
            for chunk in chunks {
                let chunk_hash = chunk_hash(&chunk.text);
                if records.contains_key(&chunk_hash) {
                    continue;
                }
                estimated_input_tokens += estimate_tokens(&chunk.text);
                let extracted = extractor
                    .extract(std::slice::from_ref(&chunk))
                    .await
                    .map_err(|error| {
                        EvalError::InvalidConfig(format!(
                            "failed to record extraction for session {} turn {} chunk {}: {error}",
                            session.session_id, turn.turn_seq, chunk.index
                        ))
                    })?;
                if options.request_delay_ms > 0 {
                    sleep(Duration::from_millis(options.request_delay_ms)).await;
                }
                let facts = extracted.iter().map(RecordedFact::from).collect::<Vec<_>>();
                estimated_output_tokens += facts
                    .iter()
                    .map(|fact| estimate_tokens(&fact.summary))
                    .sum::<usize>();
                let record = ExtractionFixtureRecord {
                    chunk_hash,
                    model: extraction_config.model.clone(),
                    prompt_version: EXTRACTION_PROMPT_VERSION.to_string(),
                    facts,
                };
                records.insert(record.chunk_hash.clone(), record);
                write_records(&output_path, &records)?;
            }
        }
    }

    let records = records.into_values().collect::<Vec<_>>();
    let chunks = records.len();
    let facts = records
        .iter()
        .map(|record| record.facts.len())
        .sum::<usize>();
    FixtureStore::write_jsonl(&output_path, records)?;
    Ok(MemoryExtractionRecordingReport {
        output_path,
        sessions: sessions.len(),
        chunks,
        facts,
        estimated_input_tokens,
        estimated_output_tokens,
        estimated_cost_usd: estimate_cost_usd(estimated_input_tokens, estimated_output_tokens),
    })
}

/// Records live LLM merge-verifier fixtures for one memory-eval corpus.
pub async fn record_memory_merges(
    options: MemoryMergeRecordingOptions,
) -> Result<MemoryMergeRecordingReport> {
    let corpus = LoadedMemoryEvalCorpus::load(&options.corpus_dir).await?;
    let output_path = options
        .output_path
        .clone()
        .unwrap_or_else(|| default_merges_path(&corpus.manifest.corpus_id));
    let extraction_path = options
        .extraction_path
        .clone()
        .unwrap_or_else(|| default_extractions_path(&corpus.manifest.corpus_id));
    let extraction_remediation = format!(
        "cargo run -p xtask -- record-memory-extractions --corpus {} --output {}",
        options.corpus_dir.display(),
        extraction_path.display()
    );
    let extraction_store = FixtureStore::<ExtractionFixtureRecord>::read_jsonl(
        &extraction_path,
        EXTRACTION_PROMPT_VERSION,
    )?
    .with_remediation_command(extraction_remediation.clone());
    let extractor = Arc::new(RecordedFactExtractor::new(
        extraction_store,
        extraction_remediation,
    ));
    let mut extraction_config = options.extraction_config.clone();
    extraction_config.api_key = load_api_key(&options.api_key_env)?;
    let live = LlmEntityMergeVerifier::from_api_key(
        extraction_config.api_key.clone(),
        &extraction_config.model,
        extraction_config.timeout_ms,
    );
    let recorder = Arc::new(RecordingEntityMergeVerifier::new(
        live,
        existing_merge_records(&output_path)?,
    ));

    let embedder =
        Arc::new(cached_embedding_provider_for_corpus(&corpus, extractor.as_ref()).await?);
    let store = IsolatedEvalStore::create().await?;
    cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
    let result = async {
        let ctx = store.ingest_ctx(embedder, extractor, recorder.clone(), true);
        super::resolve_gold_nodes(ctx, &corpus.ledger, &corpus.sessions).await?;
        cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
        let records = recorder.records().await;
        FixtureStore::write_jsonl(&output_path, records.clone())?;
        let estimated_input_tokens = records
            .iter()
            .map(|record| estimate_tokens(&record.mention) + estimate_tokens(&record.candidate))
            .sum::<usize>();
        let estimated_output_tokens = records.len();
        Ok(MemoryMergeRecordingReport {
            output_path,
            sessions: corpus.sessions.len(),
            decisions: records.len(),
            estimated_input_tokens,
            estimated_output_tokens,
            estimated_cost_usd: estimate_cost_usd(estimated_input_tokens, estimated_output_tokens),
        })
    }
    .await;
    let cleanup = store.cleanup().await;
    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            tracing::warn!(error = %cleanup_error, "failed to clean up memory merge recording store");
            Err(error)
        }
    }
}

struct RecordingEntityMergeVerifier {
    inner: LlmEntityMergeVerifier,
    records: Mutex<BTreeMap<String, EntityMergeFixtureRecord>>,
}

impl RecordingEntityMergeVerifier {
    fn new(
        inner: LlmEntityMergeVerifier,
        records: BTreeMap<String, EntityMergeFixtureRecord>,
    ) -> Self {
        Self {
            inner,
            records: Mutex::new(records),
        }
    }

    async fn records(&self) -> Vec<EntityMergeFixtureRecord> {
        self.records.lock().await.values().cloned().collect()
    }
}

#[async_trait]
impl EntityMergeVerifier for RecordingEntityMergeVerifier {
    async fn should_merge(
        &self,
        mention: &str,
        candidate: &NodeIndexRow,
    ) -> moa_memory_ingest::Result<bool> {
        let key = merge_fixture_key(mention, &candidate.name);
        if let Some(record) = self.records.lock().await.get(&key).cloned() {
            return Ok(record.should_merge);
        }
        let should_merge = self.inner.should_merge(mention, candidate).await?;
        let record = EntityMergeFixtureRecord {
            key: key.clone(),
            prompt_version: MERGE_PROMPT_VERSION.to_string(),
            mention: mention.to_string(),
            candidate: candidate.name.clone(),
            should_merge,
        };
        self.records.lock().await.insert(key, record);
        Ok(should_merge)
    }
}

fn existing_records(path: &Path) -> Result<BTreeMap<String, ExtractionFixtureRecord>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let store =
        FixtureStore::<ExtractionFixtureRecord>::read_jsonl(path, EXTRACTION_PROMPT_VERSION)?;
    Ok(store
        .records()
        .map(|record| (record.chunk_hash.clone(), record.clone()))
        .collect())
}

fn existing_merge_records(path: &Path) -> Result<BTreeMap<String, EntityMergeFixtureRecord>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let store = FixtureStore::<EntityMergeFixtureRecord>::read_jsonl(path, MERGE_PROMPT_VERSION)?;
    Ok(store
        .records()
        .map(|record| (record.key.clone(), record.clone()))
        .collect())
}

fn write_records(path: &Path, records: &BTreeMap<String, ExtractionFixtureRecord>) -> Result<()> {
    FixtureStore::write_jsonl(path, records.values().cloned())
}

fn default_extractions_path(corpus_id: &str) -> PathBuf {
    Path::new("crates/moa-eval/fixtures/memory").join(format!(
        "extractions-{corpus_id}-{EXTRACTION_PROMPT_VERSION}.jsonl"
    ))
}

fn default_merges_path(corpus_id: &str) -> PathBuf {
    Path::new("crates/moa-eval/fixtures/memory")
        .join(format!("merges-{corpus_id}-{MERGE_PROMPT_VERSION}.jsonl"))
}

fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn estimate_cost_usd(input_tokens: usize, output_tokens: usize) -> f64 {
    let input = input_tokens as f64 / 1_000_000.0 * 2.50;
    let output = output_tokens as f64 / 1_000_000.0 * 10.00;
    input + output
}
