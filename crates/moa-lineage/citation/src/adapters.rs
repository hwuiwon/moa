//! Provider-specific citation passthrough adapters.

use std::collections::HashMap;

use async_trait::async_trait;
use moa_lineage_core::{Citation, VerifierResult};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

/// One retrieved chunk as presented to a provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    /// MOA chunk or node identifier.
    pub chunk_id: Uuid,
    /// Source graph node identifier when distinct from the chunk.
    pub source_node_uid: Option<Uuid>,
    /// Source text.
    pub text: String,
    /// Provider-facing document identifier.
    pub provider_doc_id: String,
}

/// Errors returned by citation adapters.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// A provider citation mode conflicts with another request mode.
    #[error("{provider} citations are incompatible with {mode}")]
    IncompatibleMode {
        /// Provider name.
        provider: &'static str,
        /// Conflicting mode.
        mode: &'static str,
    },
    /// Provider response shape could not be interpreted.
    #[error("invalid {provider} citation response: {message}")]
    InvalidResponse {
        /// Provider name.
        provider: &'static str,
        /// Human-readable detail.
        message: String,
    },
}

/// Normalizes provider-specific citation payloads.
#[async_trait]
pub trait CitationAdapter: Send + Sync {
    /// Returns the provider identifier.
    fn provider(&self) -> &'static str;

    /// Extracts normalized citations from a raw provider response.
    async fn extract_citations(
        &self,
        provider_response: &Value,
        retrieved_chunks: &[ChunkRef],
    ) -> Result<Vec<Citation>, AdapterError>;
}

/// Anthropic Messages API citation adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicCitations;

/// OpenAI Responses/file-search annotation adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAiAnnotations;

/// Cohere chat `documents=` citation adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct CohereDocuments;

/// Vertex/Gemini grounding metadata adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct VertexGrounding;

#[async_trait]
impl CitationAdapter for AnthropicCitations {
    fn provider(&self) -> &'static str {
        "anthropic"
    }

    async fn extract_citations(
        &self,
        provider_response: &Value,
        retrieved_chunks: &[ChunkRef],
    ) -> Result<Vec<Citation>, AdapterError> {
        if provider_response
            .pointer("/request/response_format")
            .is_some()
            || provider_response
                .pointer("/request/structured_outputs")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            return Err(AdapterError::IncompatibleMode {
                provider: self.provider(),
                mode: "structured outputs",
            });
        }

        let mut out = Vec::new();
        for block in provider_response
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for citation in block
                .get("citations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(chunk) = anthropic_chunk(citation, retrieved_chunks) {
                    out.push(citation_from_chunk(
                        chunk,
                        answer_span_bytes(citation, "start_index", "end_index"),
                        cited_text(citation),
                        None,
                    ));
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl CitationAdapter for OpenAiAnnotations {
    fn provider(&self) -> &'static str {
        "openai"
    }

    async fn extract_citations(
        &self,
        provider_response: &Value,
        retrieved_chunks: &[ChunkRef],
    ) -> Result<Vec<Citation>, AdapterError> {
        let by_id = chunks_by_provider_id(retrieved_chunks);
        let mut annotations = Vec::new();
        collect_named_arrays(provider_response, "annotations", &mut annotations);

        let mut out = Vec::new();
        for annotation in annotations {
            let file_id = annotation
                .get("file_id")
                .or_else(|| annotation.get("document_id"))
                .or_else(|| annotation.pointer("/file/id"))
                .and_then(Value::as_str);
            let Some(file_id) = file_id else {
                continue;
            };
            let Some(chunk) = by_id.get(file_id) else {
                continue;
            };
            out.push(citation_from_chunk(
                chunk,
                answer_span_bytes(annotation, "start_index", "end_index"),
                cited_text(annotation),
                number_f32(annotation.get("score")),
            ));
        }
        Ok(out)
    }
}

#[async_trait]
impl CitationAdapter for CohereDocuments {
    fn provider(&self) -> &'static str {
        "cohere"
    }

    async fn extract_citations(
        &self,
        provider_response: &Value,
        retrieved_chunks: &[ChunkRef],
    ) -> Result<Vec<Citation>, AdapterError> {
        let by_id = chunks_by_provider_id(retrieved_chunks);
        let mut out = Vec::new();
        for citation in provider_response
            .get("citations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for document_id in citation
                .get("document_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if let Some(chunk) = by_id.get(document_id) {
                    out.push(citation_from_chunk(
                        chunk,
                        answer_span_bytes(citation, "start", "end"),
                        cited_text(citation),
                        number_f32(citation.get("score")),
                    ));
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl CitationAdapter for VertexGrounding {
    fn provider(&self) -> &'static str {
        "vertex"
    }

    async fn extract_citations(
        &self,
        provider_response: &Value,
        retrieved_chunks: &[ChunkRef],
    ) -> Result<Vec<Citation>, AdapterError> {
        let mut out = Vec::new();
        for metadata in grounding_metadata_values(provider_response) {
            for support in metadata
                .get("groundingSupports")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let segment = support.get("segment").unwrap_or(support);
                for idx in support
                    .get("groundingChunkIndices")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64)
                {
                    let Some(chunk) = retrieved_chunks.get(idx as usize) else {
                        continue;
                    };
                    out.push(citation_from_chunk(
                        chunk,
                        answer_span_bytes(segment, "startIndex", "endIndex"),
                        cited_text(segment),
                        None,
                    ));
                }
            }
        }
        Ok(out)
    }
}

fn citation_from_chunk(
    chunk: &ChunkRef,
    answer_span_bytes: Option<(u32, u32)>,
    cited_text: Option<String>,
    vendor_score: Option<f32>,
) -> Citation {
    Citation {
        answer_span: 0,
        answer_span_bytes,
        source_chunk_id: chunk.chunk_id,
        source_node_uid: chunk.source_node_uid,
        cited_text,
        vendor_score,
        verifier: VerifierResult {
            verified: false,
            bm25_score: None,
            nli_entailment: None,
            nli_contradiction: None,
            method: "vendor_only".to_string(),
        },
    }
}

fn anthropic_chunk<'a>(citation: &Value, chunks: &'a [ChunkRef]) -> Option<&'a ChunkRef> {
    if let Some(index) = citation
        .get("document_index")
        .or_else(|| citation.get("documentIndex"))
        .and_then(Value::as_u64)
    {
        return chunks.get(index as usize);
    }
    let doc_id = citation
        .get("document_id")
        .or_else(|| citation.get("documentId"))
        .and_then(Value::as_str)?;
    chunks.iter().find(|chunk| chunk.provider_doc_id == doc_id)
}

fn chunks_by_provider_id(chunks: &[ChunkRef]) -> HashMap<&str, &ChunkRef> {
    chunks
        .iter()
        .map(|chunk| (chunk.provider_doc_id.as_str(), chunk))
        .collect()
}

fn answer_span_bytes(value: &Value, start_key: &str, end_key: &str) -> Option<(u32, u32)> {
    let start = value.get(start_key)?.as_u64()?.min(u64::from(u32::MAX)) as u32;
    let end = value.get(end_key)?.as_u64()?.min(u64::from(u32::MAX)) as u32;
    Some((start, end))
}

fn cited_text(value: &Value) -> Option<String> {
    value
        .get("cited_text")
        .or_else(|| value.get("text"))
        .or_else(|| value.get("quote"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn number_f32(value: Option<&Value>) -> Option<f32> {
    value.and_then(Value::as_f64).map(|score| score as f32)
}

fn collect_named_arrays<'a>(value: &'a Value, name: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_named_arrays(item, name, out);
            }
        }
        Value::Object(map) => {
            if let Some(items) = map.get(name).and_then(Value::as_array) {
                out.extend(items);
            }
            for item in map.values() {
                collect_named_arrays(item, name, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn grounding_metadata_values(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    collect_object_named(value, "groundingMetadata", &mut out);
    if value.get("groundingSupports").is_some() {
        out.push(value);
    }
    out
}

fn collect_object_named<'a>(value: &'a Value, name: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_object_named(item, name, out);
            }
        }
        Value::Object(map) => {
            if let Some(item) = map.get(name) {
                out.push(item);
            }
            for item in map.values() {
                collect_object_named(item, name, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}
