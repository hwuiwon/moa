#![allow(dead_code)]
// Shared memory-eval fixture helpers.
//
// These builders and id/timestamp helpers are byte-identical across several
// `memory_eval_support/*.rs` fixtures. They live here once and are pulled into
// each test binary via a `#[path]` module declaration plus `use common::*`.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use moa_retrieval::retrieval::{LegSources, RetrievalHit, SourceTier};
use moa_core::types::security::SensitivityClass;
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::UserId,
};
use moa_eval::memory_eval::runner::QueryRewriteClassMetrics;
use moa_eval::memory_eval::{
    BootstrapConfig, CORPUS_SCHEMA_VERSION, CorpusManifest, CorpusProfile, GoldResolutionReport,
    GraphExpansionEvalPolicy, MemoryRetrievalEvalReport, ProbeResult, QueryRewritePolicy,
    RETRIEVAL_EVAL_CANDIDATE_K, RETRIEVAL_EVAL_FINAL_K, RetrievedCandidate, TranscriptStyle,
    aggregate_retrieval_eval_from_counts, candidates_from_retrieval_hits,
};
use moa_memory_graph::{NodeIndexRow, NodeLabel};
use uuid::Uuid;

pub(crate) fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp parses")
        .with_timezone(&Utc)
}

pub(crate) fn session_id(value: &str) -> SessionId {
    SessionId(Uuid::parse_str(value).expect("stable fixture session UUID"))
}

pub(crate) fn user(value: &str) -> UserId {
    UserId::new(value)
}

pub(crate) fn storage_partition(value: &str) -> StoragePartitionId {
    StoragePartitionId::new(value)
}

pub(crate) fn legs(graph: bool, vector: bool, lexical: bool) -> LegSources {
    LegSources {
        graph,
        vector,
        lexical,
    }
}

pub(crate) fn fact_ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CandidateSpec {
    pub(crate) fact_id: Option<&'static str>,
    pub(crate) legs: LegSources,
}

pub(crate) fn metric_node(uid: Uuid) -> NodeIndexRow {
    NodeIndexRow {
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some("metrics-storage-partition".to_string()),
        contact_id: Some("metrics-contact".to_string()),
        scope: "tenant".to_string(),
        name: format!("metric-node-{uid}"),
        pii_class: SensitivityClass::None,
        valid_to: None,
        valid_from: utc("2026-05-01T00:00:00Z"),
        properties_summary: None,
        last_accessed_at: utc("2026-05-02T00:00:00Z"),
        quality_score: 0.5,
    }
}

pub(crate) fn metric_candidates(base: u128, specs: &[CandidateSpec]) -> Vec<RetrievedCandidate> {
    let mut fact_ids_by_uid = HashMap::new();
    let hits = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let uid = Uuid::from_u128(base + index as u128 + 1);
            if let Some(fact_id) = spec.fact_id {
                fact_ids_by_uid.insert(uid, fact_id.to_string());
            }
            RetrievalHit {
                uid,
                score: 1.0 / (index + 1) as f64,
                legs: spec.legs,
                similarity: None,
                lexical_backend: None,
                source_tier: SourceTier::UserMemory,
                knowledge_chunk: None,
                node: metric_node(uid),
            }
        })
        .collect::<Vec<_>>();
    candidates_from_retrieval_hits(&hits, &fact_ids_by_uid, &HashMap::new(), "")
}

pub(crate) fn memory_budget_report(probe_results: Vec<ProbeResult>) -> MemoryRetrievalEvalReport {
    memory_budget_report_with_reranker(probe_results, false)
}

pub(crate) fn memory_budget_report_with_reranker(
    probe_results: Vec<ProbeResult>,
    reranker_enabled: bool,
) -> MemoryRetrievalEvalReport {
    let retrieval = aggregate_retrieval_eval_from_counts(
        3,
        3,
        probe_results,
        BootstrapConfig {
            resamples: 25,
            seed: 23,
        },
    );
    MemoryRetrievalEvalReport {
        manifest: CorpusManifest {
            version: CORPUS_SCHEMA_VERSION,
            corpus_id: "memory-budget-fixture".to_string(),
            profile: CorpusProfile::Pr,
            description: "Hermetic budget gate fixture.".to_string(),
            seeds: vec![1, 2, 3],
            transcript_style: TranscriptStyle::Marked,
        },
        candidate_k: RETRIEVAL_EVAL_CANDIDATE_K,
        final_k: RETRIEVAL_EVAL_FINAL_K,
        reranker_enabled,
        parity: false,
        query_rewrite_policy: QueryRewritePolicy::Gated,
        graph_expansion_policy: GraphExpansionEvalPolicy::Current,
        graph_retrieval_policy: GraphExpansionEvalPolicy::Current.graph_retrieval_policy(),
        graph_diagnostics: Default::default(),
        query_rewrite_call_count: 0,
        query_rewrite_skip_count: 0,
        query_rewrite_call_rate: 0.0,
        query_rewrite_p50_latency_ms: 0,
        query_rewrite_p95_latency_ms: 0,
        query_rewrite_input_tokens: 0,
        query_rewrite_output_tokens: 0,
        query_rewrite_est_usd: 0.0,
        retrieval_plus_rewrite_p95_latency_ms: retrieval.metrics.p95_retrieval_latency_ms,
        query_rewrite_by_class: BTreeMap::from([(
            "exact_identifier".to_string(),
            QueryRewriteClassMetrics {
                total_count: 1,
                call_count: 0,
                skip_count: 1,
                call_rate: 0.0,
            },
        )]),
        aborted_over_budget: false,
        cost: None,
        providers: None,
        metrics: retrieval.metrics,
        probe_results: retrieval.probe_results,
        bootstrap: retrieval.bootstrap,
        cross_user_leak_probe_ids: retrieval.cross_user_leak_probe_ids,
        gold_resolution: GoldResolutionReport {
            ingest_reports: Vec::new(),
            records: Vec::new(),
        },
        consolidation: None,
    }
}
