//! Citation adapter and cascade verifier coverage.

use moa_lineage_citation::{
    AnthropicCitations, CascadeVerifier, ChunkRef, CitationAdapter, CohereDocuments,
    OpenAiAnnotations, VertexGrounding,
};
use moa_lineage_core::Citation;
use uuid::Uuid;

#[tokio::test]
async fn anthropic_adapter_maps_document_index() {
    let chunks = chunks();
    let response = serde_json::json!({
        "content": [{
            "text": "OAuth uses tokens.",
            "citations": [{
                "document_index": 0,
                "start_index": 0,
                "end_index": 18,
                "cited_text": "OAuth uses access tokens."
            }]
        }]
    });

    let citations = AnthropicCitations
        .extract_citations(&response, &chunks)
        .await
        .expect("anthropic citation extraction should succeed");

    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0].source_chunk_id, chunks[0].chunk_id);
    assert_eq!(citations[0].answer_span_bytes, Some((0, 18)));
}

#[tokio::test]
async fn openai_adapter_maps_file_annotations() {
    let chunks = chunks();
    let response = serde_json::json!({
        "output": [{
            "content": [{
                "annotations": [{
                    "type": "file_citation",
                    "file_id": "doc-1",
                    "start_index": 4,
                    "end_index": 9,
                    "text": "tokens"
                }]
            }]
        }]
    });

    let citations = OpenAiAnnotations
        .extract_citations(&response, &chunks)
        .await
        .expect("openai citation extraction should succeed");

    assert_eq!(citations[0].source_chunk_id, chunks[0].chunk_id);
    assert_eq!(citations[0].cited_text.as_deref(), Some("tokens"));
}

#[tokio::test]
async fn cohere_adapter_maps_document_sources() {
    let chunks = chunks();
    let response = serde_json::json!({
        "citations": [{
            "start": 0,
            "end": 5,
            "text": "OAuth",
            "document_ids": ["doc-2"]
        }]
    });

    let citations = CohereDocuments
        .extract_citations(&response, &chunks)
        .await
        .expect("cohere citation extraction should succeed");

    assert_eq!(citations[0].source_chunk_id, chunks[1].chunk_id);
}

#[tokio::test]
async fn vertex_adapter_maps_grounding_supports() {
    let chunks = chunks();
    let response = serde_json::json!({
        "candidates": [{
            "groundingMetadata": {
                "groundingSupports": [{
                    "segment": { "startIndex": 0, "endIndex": 5, "text": "OAuth" },
                    "groundingChunkIndices": [1]
                }]
            }
        }]
    });

    let citations = VertexGrounding
        .extract_citations(&response, &chunks)
        .await
        .expect("vertex citation extraction should succeed");

    assert_eq!(citations[0].source_chunk_id, chunks[1].chunk_id);
    assert_eq!(citations[0].answer_span_bytes, Some((0, 5)));
}

#[tokio::test]
async fn cascade_flags_vendor_hallucinated_citation() {
    let chunks = chunks();
    let citation = Citation {
        answer_span: 0,
        answer_span_bytes: None,
        source_chunk_id: chunks[1].chunk_id,
        source_node_uid: None,
        cited_text: Some("unrelated".to_string()),
        vendor_score: None,
        verifier: moa_lineage_core::VerifierResult {
            verified: true,
            bm25_score: None,
            nli_entailment: None,
            nli_contradiction: None,
            method: "vendor_only".to_string(),
        },
    };

    let verified = CascadeVerifier::bm25_only()
        .verify_all(
            "OAuth uses access tokens.",
            &[(0, 25)],
            &[citation],
            &chunks,
        )
        .await;

    assert_eq!(verified.len(), 1);
    assert!(!verified[0].verifier.verified);
}

fn chunks() -> Vec<ChunkRef> {
    vec![
        ChunkRef {
            chunk_id: Uuid::now_v7(),
            source_node_uid: None,
            text: "OAuth uses access tokens for delegated authorization.".to_string(),
            provider_doc_id: "doc-1".to_string(),
        },
        ChunkRef {
            chunk_id: Uuid::now_v7(),
            source_node_uid: None,
            text: "Postgres indexes speed up lineage queries.".to_string(),
            provider_doc_id: "doc-2".to_string(),
        },
    ]
}
