//! Citation adapter and cascade verifier coverage.
//!
//! These tests pin two contracts:
//! 1. The provider adapters normalize known citation shapes and stay lenient
//!    (skip → empty) on unknown ids, while the structured-output guard surfaces
//!    a typed `AdapterError`.
//! 2. The BM25 cascade distinguishes a grounded answer sentence (verified) from
//!    an unsupported one, instead of accepting any vendor-claimed citation.

use moa_lineage_citation::{
    AdapterError, AnthropicCitations, CascadeVerifier, ChunkRef, CitationAdapter, CitationVerifier,
    CohereDocuments, OpenAiAnnotations, VerificationInput, VertexGrounding,
};
use moa_lineage_core::{Citation, VerifierResult};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn anthropic_adapter_maps_document_index() {
    let chunks = chunks();
    let response = json!({
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
    let response = json!({
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
    let response = json!({
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
    let response = json!({
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

// --- Adapter negative-space coverage -------------------------------------

#[tokio::test]
async fn anthropic_adapter_rejects_structured_output_mode() {
    let chunks = chunks();
    let response = json!({
        "request": { "response_format": { "type": "json_schema" } },
        "content": [{
            "citations": [{ "document_index": 0, "start_index": 0, "end_index": 5 }]
        }]
    });

    let error = AnthropicCitations
        .extract_citations(&response, &chunks)
        .await
        .expect_err("structured outputs must be incompatible with passthrough citations");

    assert!(
        matches!(
            error,
            AdapterError::IncompatibleMode {
                provider: "anthropic",
                mode: "structured outputs"
            }
        ),
        "expected IncompatibleMode for structured outputs, got {error:?}"
    );
}

#[tokio::test]
async fn anthropic_adapter_rejects_structured_outputs_bool_flag() {
    let chunks = chunks();
    let response = json!({
        "request": { "structured_outputs": true },
        "content": [{
            "citations": [{ "document_index": 0, "start_index": 0, "end_index": 5 }]
        }]
    });

    let error = AnthropicCitations
        .extract_citations(&response, &chunks)
        .await
        .expect_err("structured_outputs=true must be incompatible with citations");

    assert!(
        matches!(
            error,
            AdapterError::IncompatibleMode {
                provider: "anthropic",
                ..
            }
        ),
        "expected IncompatibleMode, got {error:?}"
    );
}

#[tokio::test]
async fn anthropic_adapter_skips_out_of_range_document_index() {
    let chunks = chunks();
    let response = json!({
        "content": [{
            "citations": [{ "document_index": 99, "start_index": 0, "end_index": 5 }]
        }]
    });

    let citations = AnthropicCitations
        .extract_citations(&response, &chunks)
        .await
        .expect("an unknown document index should not be an error");

    assert!(
        citations.is_empty(),
        "out-of-range document_index must map to zero citations, got {citations:?}"
    );
}

#[tokio::test]
async fn anthropic_adapter_falls_back_to_document_id() {
    let chunks = chunks();
    let response = json!({
        "content": [{
            "citations": [{
                "document_id": "doc-2",
                "start_index": 0,
                "end_index": 5,
                "cited_text": "OAuth"
            }]
        }]
    });

    let citations = AnthropicCitations
        .extract_citations(&response, &chunks)
        .await
        .expect("document_id fallback should resolve");

    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0].source_chunk_id, chunks[1].chunk_id);
}

#[tokio::test]
async fn openai_adapter_skips_unknown_file_id() {
    let chunks = chunks();
    let response = json!({
        "output": [{
            "content": [{
                "annotations": [{
                    "type": "file_citation",
                    "file_id": "missing-doc",
                    "start_index": 0,
                    "end_index": 1
                }]
            }]
        }]
    });

    let citations = OpenAiAnnotations
        .extract_citations(&response, &chunks)
        .await
        .expect("unknown file id should not be an error");

    assert!(
        citations.is_empty(),
        "annotation referencing an unknown file id must be dropped, got {citations:?}"
    );
}

#[tokio::test]
async fn cohere_adapter_skips_unknown_document_ids() {
    let chunks = chunks();
    let response = json!({
        "citations": [{ "start": 0, "end": 5, "document_ids": ["missing-doc"] }]
    });

    let citations = CohereDocuments
        .extract_citations(&response, &chunks)
        .await
        .expect("unknown document ids should not be an error");

    assert!(
        citations.is_empty(),
        "citation referencing an unknown document id must be dropped, got {citations:?}"
    );
}

#[tokio::test]
async fn vertex_adapter_skips_out_of_range_chunk_index() {
    let chunks = chunks();
    let response = json!({
        "candidates": [{
            "groundingMetadata": {
                "groundingSupports": [{
                    "segment": { "startIndex": 0, "endIndex": 5 },
                    "groundingChunkIndices": [99]
                }]
            }
        }]
    });

    let citations = VertexGrounding
        .extract_citations(&response, &chunks)
        .await
        .expect("an out-of-range grounding chunk index should not be an error");

    assert!(
        citations.is_empty(),
        "out-of-range groundingChunkIndices must map to zero citations, got {citations:?}"
    );
}

#[tokio::test]
async fn adapters_return_empty_for_unparseable_response() {
    // Adapters are intentionally lenient: a wholly-unexpected payload yields no
    // citations rather than an error. `AdapterError::InvalidResponse` is reserved
    // for a future strict-parsing mode and is not produced by any adapter today,
    // so the observable contract on garbage input is `Ok([])`.
    let chunks = chunks();
    let garbage = json!({ "unexpected": [1, 2, 3], "nested": { "noise": true } });

    let adapters: Vec<Box<dyn CitationAdapter>> = vec![
        Box::new(AnthropicCitations),
        Box::new(OpenAiAnnotations),
        Box::new(CohereDocuments),
        Box::new(VertexGrounding),
    ];
    for adapter in adapters {
        let citations = adapter
            .extract_citations(&garbage, &chunks)
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{} should tolerate an unparseable response, got {error:?}",
                    adapter.provider()
                )
            });
        assert!(
            citations.is_empty(),
            "{} should produce no citations from garbage, got {citations:?}",
            adapter.provider()
        );
    }
}

// --- Cascade verifier: grounded vs hallucinated --------------------------

#[tokio::test]
async fn cascade_bm25_verify_grounds_supported_chunk_and_rejects_unrelated_chunk() {
    // Direct `verify()` over >=2 candidates exercises the BM25 gate
    // (`bm25_min_candidates`) and the positive `verified: score > 5.0` branch.
    let chunks = oauth_corpus();
    let results = CascadeVerifier::bm25_only()
        .verify(VerificationInput {
            answer_sentence: "OAuth access tokens authorize delegated API requests securely.",
            candidate_chunks: &chunks,
        })
        .await;

    assert!(
        results.len() >= 2,
        "the answer must lexically touch >=2 candidates so the BM25 gate runs, got {} hits",
        results.len()
    );

    let grounded = results
        .iter()
        .find(|(id, _)| *id == chunks[0].chunk_id)
        .expect("the strongly-overlapping chunk should be scored");
    let grounded_score = grounded
        .1
        .bm25_score
        .expect("a scored chunk carries a bm25 score");
    assert!(
        grounded.1.verified,
        "the strongly grounded chunk should verify, got {:?}",
        grounded.1
    );
    assert!(
        grounded_score > 5.0,
        "grounded bm25 score must clear the 5.0 threshold, got {grounded_score}"
    );

    let weak = results
        .iter()
        .find(|(id, _)| *id == chunks[1].chunk_id)
        .expect("the weakly-overlapping chunk should be scored");
    let weak_score = weak
        .1
        .bm25_score
        .expect("a scored chunk carries a bm25 score");
    assert!(
        !weak.1.verified,
        "a weakly-overlapping chunk must not verify, got {:?}",
        weak.1
    );
    assert!(
        weak_score <= 5.0,
        "weak bm25 score must stay under the threshold, got {weak_score}"
    );
}

#[tokio::test]
async fn cascade_verify_all_uncited_grounds_supported_sentence_and_skips_ungrounded_sentence() {
    // With no provider citations, `verify_all` routes through
    // `verify_uncited_answer`, which scores every sentence against all retrieved
    // chunks. The grounded sentence is synthesized into a verified citation; the
    // ungrounded sentence has zero lexical support and is dropped.
    let chunks = oauth_corpus();
    let grounded = "OAuth access tokens authorize delegated API requests securely.";
    let ungrounded = "Photosynthesis converts sunlight into glucose within plant chloroplasts.";
    let answer = format!("{grounded} {ungrounded}");
    let grounded_end = u32::try_from(grounded.len()).expect("sentence length fits u32");
    let answer_end = u32::try_from(answer.len()).expect("answer length fits u32");
    let offsets = [(0u32, grounded_end), (grounded_end + 1, answer_end)];

    let citations = CascadeVerifier::bm25_only()
        .verify_all(&answer, &offsets, &[], &chunks)
        .await;

    assert_eq!(
        citations.len(),
        1,
        "only the grounded sentence should be cited, got {citations:?}"
    );
    let citation = &citations[0];
    assert_eq!(
        citation.answer_span, 0,
        "the citation should index sentence 0"
    );
    assert_eq!(
        citation.source_chunk_id, chunks[0].chunk_id,
        "the synthesized citation should point at the grounding chunk"
    );
    assert!(
        citation.verifier.verified,
        "the grounded synthesized citation should verify, got {:?}",
        citation.verifier
    );
    assert!(
        citation.verifier.bm25_score.unwrap_or(0.0) > 5.0,
        "the grounded citation's bm25 score should clear the threshold, got {:?}",
        citation.verifier.bm25_score
    );
}

#[tokio::test]
async fn cascade_flags_vendor_hallucinated_citation() {
    // A provider can self-report a citation as `verified: true`. The cascade must
    // re-verify against the retrieved corpus instead of trusting that flag. Here
    // the vendor points an OAuth answer at the unrelated Postgres chunk, so the
    // cascade overrides the claim to unverified.
    let chunks = oauth_corpus();
    let hallucinated = Citation {
        answer_span: 0,
        answer_span_bytes: None,
        source_chunk_id: chunks[2].chunk_id,
        source_node_uid: None,
        cited_text: Some(
            "Postgres indexes accelerate analytical query execution plans.".to_string(),
        ),
        vendor_score: Some(0.99),
        verifier: VerifierResult {
            verified: true,
            bm25_score: None,
            nli_entailment: None,
            nli_contradiction: None,
            method: "vendor_only".to_string(),
        },
    };
    let answer = "OAuth access tokens authorize delegated API requests securely.";
    let offsets = [(0u32, u32::try_from(answer.len()).expect("fits u32"))];

    let verified = CascadeVerifier::bm25_only()
        .verify_all(
            answer,
            &offsets,
            std::slice::from_ref(&hallucinated),
            &chunks,
        )
        .await;

    assert_eq!(verified.len(), 1);
    assert!(
        !verified[0].verifier.verified,
        "the cascade must override a vendor-claimed citation that is not grounded"
    );
    assert_ne!(
        verified[0].verifier.method, "vendor_only",
        "the cascade should re-stamp the method after re-verifying, got {:?}",
        verified[0].verifier.method
    );
}

/// Two chunks used by the provider-adapter mapping tests.
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

/// Four topically-distinct chunks for grounding tests.
///
/// `[0]` strongly supports the OAuth answer sentence, `[1]` overlaps only on the
/// common tokens "tokens"/"requests" (so it enters BM25 scoring but stays under
/// the verification threshold), and `[2]`/`[3]` are unrelated.
fn oauth_corpus() -> Vec<ChunkRef> {
    vec![
        ChunkRef {
            chunk_id: Uuid::now_v7(),
            source_node_uid: None,
            text: "OAuth access tokens authorize delegated API requests for third-party clients securely."
                .to_string(),
            provider_doc_id: "oauth".to_string(),
        },
        ChunkRef {
            chunk_id: Uuid::now_v7(),
            source_node_uid: None,
            text: "Session tokens expire after inactivity to limit replay requests.".to_string(),
            provider_doc_id: "session".to_string(),
        },
        ChunkRef {
            chunk_id: Uuid::now_v7(),
            source_node_uid: None,
            text: "Postgres indexes accelerate analytical query execution plans.".to_string(),
            provider_doc_id: "postgres".to_string(),
        },
        ChunkRef {
            chunk_id: Uuid::now_v7(),
            source_node_uid: None,
            text: "Kubernetes schedules container workloads across worker nodes.".to_string(),
            provider_doc_id: "kubernetes".to_string(),
        },
    ]
}
