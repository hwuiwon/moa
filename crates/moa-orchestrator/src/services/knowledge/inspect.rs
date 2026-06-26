//! Safe source-object inspection and query-trace projection logic for Knowledge.

use chrono::Utc;
use moa_core::wire::knowledge::{
    KnowledgeObjectChunkInspectView, KnowledgeObjectInspectRequest, KnowledgeObjectInspectResponse,
    KnowledgeObjectListRequest, KnowledgeObjectListResponse, KnowledgeQueryTraceRequest,
    KnowledgeQueryTraceResponse,
};
use moa_knowledge::normalize::redact_provider_metadata;
use serde_json::{Value, json};

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

    /// Returns a renderer-safe empty query trace until full lineage hydration lands.
    pub async fn query_trace(
        &self,
        request: KnowledgeQueryTraceRequest,
    ) -> Result<KnowledgeQueryTraceResponse, KnowledgeServiceError> {
        Ok(KnowledgeQueryTraceResponse {
            trace_uid: request.trace_uid,
            original_query: String::new(),
            retrieval_query: None,
            searched_scopes: Vec::new(),
            stages: Vec::new(),
            hits: Vec::new(),
            created_at: Utc::now(),
        })
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
