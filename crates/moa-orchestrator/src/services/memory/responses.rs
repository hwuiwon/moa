//! Wire response builders for memory retrieval and ingestion results.

use moa_brain::retrieval::RetrievalHit;
use moa_core::wire::memory::{MemoryHit, MemoryIngestResult};
use moa_memory_graph::NodeIndexRow;
use moa_memory_ingest::IngestApplyReport;
use serde_json::Value;

/// Converts one retrieval hit into the public memory-hit DTO.
pub(super) fn memory_hit_from_retrieval(hit: RetrievalHit) -> MemoryHit {
    let chunk = hit.knowledge_chunk.as_ref();
    MemoryHit {
        uid: hit.uid,
        label: hit.node.label.as_str().to_string(),
        name: hit.node.name.clone(),
        score: hit.score,
        snippet: chunk
            .map(|chunk| chunk.text.clone())
            .unwrap_or_else(|| node_snippet(&hit.node)),
        legs: leg_trace(hit.legs),
        chunk_uid: chunk.map(|chunk| chunk.chunk_uid),
        document_version_uid: chunk.map(|chunk| chunk.document_version_uid),
        source_uri: chunk.and_then(|chunk| chunk.source_uri.clone()),
        source_title: chunk.and_then(|chunk| chunk.source_title.clone()),
        properties: hit.node.properties_summary,
    }
}

fn leg_trace(legs: moa_brain::retrieval::LegSources) -> Vec<String> {
    let mut out = Vec::new();
    if legs.graph {
        out.push("graph".to_string());
    }
    if legs.vector {
        out.push("vector".to_string());
    }
    if legs.lexical {
        out.push("lexical".to_string());
    }
    out
}

fn node_snippet(node: &NodeIndexRow) -> String {
    let Some(properties) = &node.properties_summary else {
        return String::new();
    };
    if let Some(value) = properties.get("summary").and_then(Value::as_str) {
        return value.to_string();
    }
    if let Some(value) = properties.get("object").and_then(Value::as_str) {
        return value.to_string();
    }
    properties.to_string()
}

/// Converts an ingestion apply report into the public ingest result DTO.
pub(super) fn ingest_result_from_report(
    source_name: String,
    report: IngestApplyReport,
) -> MemoryIngestResult {
    MemoryIngestResult {
        source_name,
        inserted: usize_to_u64(report.inserted),
        superseded: usize_to_u64(report.superseded),
        skipped: usize_to_u64(report.skipped),
        failed: usize_to_u64(report.failed),
        edges: 0,
        contradictions: 0,
        dead_lettered: report.failed > 0,
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
