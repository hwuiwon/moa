//! Memory evaluation corpus, fixture, and scoring support.

pub mod corpus;
pub mod embeddings;
pub mod generator;
pub mod gold;
pub mod judge;
pub mod metrics;
pub mod recording;
pub mod runner;

pub use crate::kernel::{
    BinaryProbeOutcome, BootstrapConfig, ClusterBootstrapReport, ClusterObservation,
    DEFAULT_BOOTSTRAP_RESAMPLES, MetricSummary, PairedComparison, PerLegRecall, benjamini_hochberg,
    cluster_bootstrap_mean_by_user, mcnemar_paired_test,
};
pub use corpus::{
    CORPUS_SCHEMA_VERSION, CorpusManifest, CorpusProfile, LedgerFact, Probe, ProbeType,
    SyntheticSession, SyntheticTurn, TranscriptStyle, read_ledger_jsonl, read_manifest_json,
    read_probes_jsonl, read_sessions_jsonl, validate_corpus, validate_ledger, validate_probes,
    validate_sessions, write_ledger_jsonl, write_manifest_json, write_probes_jsonl,
    write_sessions_jsonl,
};
pub use embeddings::{
    CACHED_EMBEDDING_MODEL, CachedEmbeddingFixture, CachedEmbeddingProvider,
    build_cached_embedding_fixtures, embedding_text_hash, read_embeddings_jsonl,
    validate_embedding_fixtures, write_embeddings_jsonl,
};
pub use generator::{
    EmbeddingInput, EmbeddingInputKind, GeneratedMemoryEvalCorpus, generate_memory_eval_corpus,
    generate_memory_eval_corpus_with_style, read_embedding_inputs_jsonl,
    write_embedding_inputs_jsonl, write_memory_eval_corpus,
};
pub use gold::{
    GoldIngestTurnReport, GoldNodeRecord, GoldNodeSnapshot, GoldPiiStatus, GoldResolutionReport,
    GoldResolutionStatus, ScopeMatchBreakdown, read_gold_nodes_jsonl, resolve_gold_nodes,
    write_gold_nodes_jsonl,
};
pub use judge::{
    AnswerJudge, DeterministicJudge, JudgeInput, JudgeOutcome, PairwiseLlmJudge, PairwiseWinner,
};
pub use metrics::{
    CandidateLegs, ExtractionPrecisionCounts, ProbeResult, RetrievalEvalReport, RetrievalMetrics,
    RetrievedCandidate, aggregate_retrieval_eval, aggregate_retrieval_eval_from_counts,
    aggregate_retrieval_eval_from_diagnostic_counts,
    aggregate_retrieval_eval_with_extraction_precision, candidates_from_retrieval_hits,
};
pub use moa_brain::retrieval::{RankingConfig, RankingMode, RankingWeights};
pub use recording::{
    MemoryExtractionRecordingOptions, MemoryExtractionRecordingReport, record_memory_extractions,
};
pub use runner::{
    MemoryEvalExtractorMode, MemoryRetrievalEvalOptions, MemoryRetrievalEvalReport,
    RETRIEVAL_EVAL_CANDIDATE_K, RETRIEVAL_EVAL_FINAL_K, run_memory_retrieval_eval,
};

impl crate::kernel::FixtureRecord for moa_memory_ingest::ExtractionFixtureRecord {
    fn fixture_key(&self) -> &str {
        self.key()
    }

    fn fixture_version(&self) -> &str {
        self.version()
    }
}

impl moa_memory_ingest::RecordedExtractionStore
    for crate::kernel::FixtureStore<moa_memory_ingest::ExtractionFixtureRecord>
{
    fn get_optional(&self, key: &str) -> Option<&moa_memory_ingest::ExtractionFixtureRecord> {
        crate::kernel::FixtureStore::get_optional(self, key)
    }
}
