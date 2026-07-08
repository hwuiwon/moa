//! Safe source-object inspection and query-trace projection logic for Knowledge.

use std::time::Duration;

use chrono::Utc;
use moa_core::wire::knowledge::{
    KnowledgeObjectChunkInspectView, KnowledgeObjectInspectRequest, KnowledgeObjectInspectResponse,
    KnowledgeObjectListRequest, KnowledgeObjectListResponse, KnowledgeQueryTraceHit,
    KnowledgeQueryTraceRequest, KnowledgeQueryTraceResponse, KnowledgeQueryTraceStage,
};
use moa_core::{MoaError, StoragePartitionId};
use moa_knowledge::normalize::redact_provider_metadata;
use moa_lineage_core::{LineageEvent, RecordKind, RetrievalLineage};
use moa_observability::{record_knowledge_retrieval_duration, record_knowledge_retrieval_hits};
use serde_json::{Value, json};
use sqlx::Row;

use super::{KnowledgeService, KnowledgeServiceError, sync::step_view};

impl KnowledgeService {
    /// Lists source objects with parser and graph counters, without raw content.
    pub async fn list_objects(
        &self,
        request: KnowledgeObjectListRequest,
    ) -> Result<KnowledgeObjectListResponse, KnowledgeServiceError> {
        let limit = request.limit.unwrap_or(100).min(500);
        let objects = self
            .repository(request.tenant_id)
            .list_objects(
                request.tenant_id,
                request.connection_uid,
                request.object_type.as_deref(),
                limit,
            )
            .await?
            .into_iter()
            .map(|projection| {
                let object = projection.object;
                json!({
                    "object_uid": object.object_uid,
                    "connection_uid": object.connection_uid,
                    "object_type": object.object_type,
                    "source_id": object.source_id,
                    "source_uri": object.source_uri,
                    "title": object.title,
                    "status": object.status.as_str(),
                    "source_updated_at": object.source_updated_at,
                    "deleted_at": object.deleted_at,
                    "parser": projection.parser,
                    "parser_status": projection.parser_status,
                    "chunk_count": projection.chunk_count,
                    "graph_node_count": projection.graph_node_count,
                    "metadata": redact_provider_metadata(object.metadata),
                })
            })
            .collect();

        Ok(KnowledgeObjectListResponse {
            objects,
            next_cursor: None,
        })
    }

    /// Inspects one source object with bounded previews and redacted metadata only.
    pub async fn inspect_object(
        &self,
        request: KnowledgeObjectInspectRequest,
    ) -> Result<KnowledgeObjectInspectResponse, KnowledgeServiceError> {
        let inspection = self
            .repository(request.tenant_id)
            .inspect_object(request.object_uid)
            .await?
            .ok_or(KnowledgeServiceError::NotFound("knowledge object"))?;
        if inspection.object.tenant_id != request.tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge object"));
        }

        let chunk_preview_limit = self.max_preview_chars.min(512);
        let chunks = inspection
            .chunks
            .iter()
            .map(|chunk| KnowledgeObjectChunkInspectView {
                chunk_uid: chunk.chunk_uid,
                ordinal: chunk.ordinal,
                chunk_hash: chunk.chunk_hash.clone(),
                heading_path: chunk.heading_path.clone(),
                token_count: chunk.token_count,
                preview: bounded_preview(&chunk.text, chunk_preview_limit),
                graph_node_uid: chunk.graph_node_uid,
                metadata: redact_provider_metadata(chunk.metadata.clone()),
            })
            .collect::<Vec<_>>();
        let heading_paths = unique_heading_paths(&chunks);
        let graph_node_uids = chunks
            .iter()
            .filter_map(|chunk| chunk.graph_node_uid)
            .collect::<Vec<_>>();
        let object_preview = if inspection.chunks.is_empty() {
            inspection.object.title.clone()
        } else {
            Some(bounded_preview(
                &inspection
                    .chunks
                    .iter()
                    .map(|chunk| chunk.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                self.max_preview_chars,
            ))
        };
        let citation_metadata = json!({
            "chunks": chunks
                .iter()
                .map(|chunk| json!({
                    "chunk_uid": chunk.chunk_uid,
                    "graph_node_uid": chunk.graph_node_uid,
                    "heading_path": chunk.heading_path,
                    "metadata": chunk.metadata,
                }))
                .collect::<Vec<_>>()
        });

        Ok(KnowledgeObjectInspectResponse {
            object_uid: inspection.object.object_uid,
            object_type: inspection.object.object_type,
            source_id: inspection.object.source_id,
            status: inspection.object.status.as_str().to_string(),
            preview: object_preview,
            version_uid: inspection
                .version
                .as_ref()
                .map(|version| version.version_uid),
            parser: inspection
                .version
                .as_ref()
                .map(|version| version.parser.clone()),
            parser_metadata: inspection
                .version
                .as_ref()
                .map(|version| redact_provider_metadata(version.metadata.clone()))
                .unwrap_or(Value::Null),
            heading_paths,
            chunks,
            graph_node_uids,
            citation_metadata,
            metadata: redact_provider_metadata(inspection.object.metadata),
            steps: inspection.steps.into_iter().map(step_view).collect(),
        })
    }

    /// Returns a renderer-safe retrieval trace from durable lineage rows.
    #[tracing::instrument(
        name = "knowledge_retrieval_trace",
        skip(self, request),
        fields(
            tenant_id = %request.tenant_id,
            connection_id = "none",
            sync_run_id = "none",
            provider = "retrieval",
            parser = "none",
            trace_id = %request.trace_uid,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            stage_count = tracing::field::Empty,
            hit_count = tracing::field::Empty
        )
    )]
    pub async fn query_trace(
        &self,
        request: KnowledgeQueryTraceRequest,
    ) -> Result<KnowledgeQueryTraceResponse, KnowledgeServiceError> {
        let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id);
        let payloads: Vec<Value> = if let Some(clickhouse) = self.lineage_clickhouse.as_deref() {
            clickhouse
                .trace_payloads(
                    &storage_partition_id,
                    request.trace_uid,
                    RecordKind::Retrieval.as_i16(),
                )
                .await
                .map_err(|error| {
                    tracing::Span::current().record("status", "failed");
                    tracing::Span::current().record("error_code", "query_trace_storage_failed");
                    KnowledgeServiceError::Moa(MoaError::StorageError(error.to_string()))
                })?
                .into_iter()
                .map(|(_, payload)| payload)
                .collect()
        } else {
            let Some(pool) = self.postgres_pool() else {
                tracing::Span::current().record("status", "no_pool");
                tracing::Span::current().record("error_code", "none");
                tracing::Span::current().record("stage_count", 0_u64);
                tracing::Span::current().record("hit_count", 0_u64);
                return Ok(empty_query_trace_response(request.trace_uid));
            };
            let rows = sqlx::query(
                r#"
                SELECT ts, payload
                FROM analytics.turn_lineage
                WHERE storage_partition_id = $1
                  AND turn_id = $2
                  AND record_kind = $3
                ORDER BY ts ASC, record_kind ASC
                "#,
            )
            .bind(storage_partition_id.as_str())
            .bind(request.trace_uid)
            .bind(RecordKind::Retrieval.as_i16())
            .fetch_all(&pool)
            .await
            .map_err(|error| {
                tracing::Span::current().record("status", "failed");
                tracing::Span::current().record("error_code", "query_trace_storage_failed");
                KnowledgeServiceError::Moa(MoaError::StorageError(error.to_string()))
            })?;
            rows.into_iter()
                .map(|row| {
                    row.try_get::<Value, _>("payload").map_err(|error| {
                        tracing::Span::current().record("status", "failed");
                        tracing::Span::current().record("error_code", "query_trace_decode_failed");
                        KnowledgeServiceError::Moa(MoaError::StorageError(error.to_string()))
                    })
                })
                .collect::<Result<_, _>>()?
        };

        let mut traces = Vec::with_capacity(payloads.len());
        for payload in payloads {
            if let LineageEvent::Retrieval(record) = serde_json::from_value::<LineageEvent>(payload)
                .map_err(|error| {
                    tracing::Span::current().record("status", "failed");
                    tracing::Span::current().record("error_code", "query_trace_payload_invalid");
                    KnowledgeServiceError::InvalidRequest(format!(
                        "invalid retrieval lineage payload: {error}"
                    ))
                })?
            {
                traces.push(record);
            }
        }
        let response = render_query_trace_response(request.trace_uid, traces);
        tracing::Span::current().record("status", "success");
        tracing::Span::current().record("error_code", "none");
        tracing::Span::current().record("stage_count", response.stages.len() as u64);
        tracing::Span::current().record("hit_count", response.hits.len() as u64);
        record_query_trace_metrics(&response);
        Ok(response)
    }
}

fn empty_query_trace_response(trace_uid: uuid::Uuid) -> KnowledgeQueryTraceResponse {
    KnowledgeQueryTraceResponse {
        trace_uid,
        original_query: String::new(),
        retrieval_query: None,
        searched_scopes: Vec::new(),
        stages: Vec::new(),
        hits: Vec::new(),
        created_at: Utc::now(),
    }
}

fn render_query_trace_response(
    trace_uid: uuid::Uuid,
    traces: Vec<RetrievalLineage>,
) -> KnowledgeQueryTraceResponse {
    let Some(first) = traces.first() else {
        return empty_query_trace_response(trace_uid);
    };
    let mut searched_scopes = Vec::new();
    let mut stages = Vec::new();
    let mut hits = Vec::new();
    for record in &traces {
        extend_unique(&mut searched_scopes, record.searched_scopes.iter().cloned());
        stages.extend(trace_stages(record));
        hits.extend(
            record
                .selected_hits
                .iter()
                .map(|hit| KnowledgeQueryTraceHit {
                    uid: hit.chunk_uid.unwrap_or(hit.graph_node_uid),
                    source_tier: hit.source_tier.clone(),
                    label: hit.label.clone(),
                    title: hit.title.clone(),
                    snippet: hit.snippet.clone(),
                    score: hit.score,
                    citation: trace_hit_citation(hit),
                }),
        );
    }

    KnowledgeQueryTraceResponse {
        trace_uid,
        original_query: first.query_original.clone(),
        retrieval_query: first.query_expansions.first().cloned(),
        searched_scopes,
        stages,
        hits,
        created_at: first.ts,
    }
}

fn record_query_trace_metrics(response: &KnowledgeQueryTraceResponse) {
    for stage in &response.stages {
        record_knowledge_retrieval_duration(
            &stage.stage,
            "success",
            Duration::from_millis(stage.latency_ms),
        );
    }
    for hit in &response.hits {
        let legs = hit
            .citation
            .get("legs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for leg in legs {
            if let Some(leg) = leg.as_str() {
                record_knowledge_retrieval_hits(&hit.source_tier, leg, 1);
            }
        }
    }
}

fn trace_stages(record: &RetrievalLineage) -> Vec<KnowledgeQueryTraceStage> {
    let mut stages = Vec::new();
    push_trace_stage(&mut stages, "embed", 0, record.timings.embed_ms, json!({}));
    push_trace_stage(
        &mut stages,
        "vector",
        record.vector_hits.len(),
        record.timings.vector_search_ms,
        json!({}),
    );
    push_trace_stage(
        &mut stages,
        "graph",
        record.graph_paths.len(),
        record.timings.graph_search_ms,
        json!({}),
    );
    push_trace_stage(
        &mut stages,
        "lexical",
        lexical_candidate_count(record),
        record.timings.lexical_search_ms,
        json!({}),
    );
    push_trace_stage(
        &mut stages,
        "fusion",
        record.fusion_scores.len(),
        record.timings.fusion_ms,
        json!({ "filters": record.filters }),
    );
    push_trace_stage(
        &mut stages,
        "reranker",
        record.rerank_scores.len(),
        record.timings.rerank_ms,
        json!({}),
    );
    push_trace_stage(
        &mut stages,
        "context",
        record
            .selected_hits
            .iter()
            .filter(|hit| hit.prompt_included)
            .count(),
        record.timings.total_ms,
        json!({ "top_k": record.top_k }),
    );
    stages
}

fn push_trace_stage(
    stages: &mut Vec<KnowledgeQueryTraceStage>,
    stage: &str,
    candidate_count: usize,
    latency_ms: u32,
    metadata: Value,
) {
    if candidate_count == 0 && latency_ms == 0 && metadata == json!({}) {
        return;
    }
    stages.push(KnowledgeQueryTraceStage {
        stage: stage.to_string(),
        candidate_count: candidate_count.min(u32::MAX as usize) as u32,
        latency_ms: u64::from(latency_ms),
        metadata,
    });
}

fn lexical_candidate_count(record: &RetrievalLineage) -> usize {
    record
        .fusion_scores
        .iter()
        .filter(|score| score.lexical_contribution > 0.0)
        .count()
}

fn trace_hit_citation(hit: &moa_lineage_core::RetrievalSelectedHit) -> Value {
    let mut citation = match redact_provider_metadata(hit.citation.clone()) {
        Value::Object(map) => Value::Object(map),
        _ => json!({}),
    };
    if let Value::Object(object) = &mut citation {
        object.insert("graph_node_uid".to_string(), json!(hit.graph_node_uid));
        object.insert("chunk_uid".to_string(), json!(hit.chunk_uid));
        object.insert("fact_uid".to_string(), json!(hit.fact_uid));
        object.insert("legs".to_string(), json!(hit.legs));
        if let Some(source_uri) = &hit.source_uri {
            object.insert("source_uri".to_string(), json!(source_uri));
        }
        if let Some(source_title) = &hit.source_title {
            object.insert("source_title".to_string(), json!(source_title));
        }
    }
    citation
}

fn extend_unique(target: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

fn bounded_preview(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut preview = input.chars().take(max_chars).collect::<String>();
    if input.chars().count() > max_chars {
        preview.push_str("...");
    }
    preview
}

fn unique_heading_paths(chunks: &[KnowledgeObjectChunkInspectView]) -> Vec<Vec<String>> {
    let mut paths = Vec::new();
    for chunk in chunks {
        if !chunk.heading_path.is_empty() && !paths.contains(&chunk.heading_path) {
            paths.push(chunk.heading_path.clone());
        }
    }
    paths
}
