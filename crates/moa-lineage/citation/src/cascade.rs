//! Two-stage cascade verifier.

use std::collections::BTreeMap;

use async_trait::async_trait;
use moa_lineage_core::{Citation, VerifierResult};
use uuid::Uuid;

use crate::adapters::ChunkRef;
use crate::verifiers::{Bm25Verifier, CitationVerifier, NliVerifier, VerificationInput};

/// Cascade verifier configuration.
#[derive(Clone, Debug)]
pub struct CascadeConfig {
    /// Number of BM25 candidates to retain.
    pub bm25_top_k: usize,
    /// Minimum candidates required before NLI runs.
    pub bm25_min_candidates: usize,
    /// Minimum entailment score for verified citations.
    pub nli_threshold: f32,
    /// Maximum NLI concurrency. Reserved for the ONNX-backed verifier.
    pub max_concurrent_nli: usize,
}

impl Default for CascadeConfig {
    fn default() -> Self {
        Self {
            bm25_top_k: 3,
            bm25_min_candidates: 2,
            nli_threshold: 0.5,
            max_concurrent_nli: 4,
        }
    }
}

/// BM25 then optional NLI verifier.
#[derive(Clone)]
pub struct CascadeVerifier {
    bm25: Bm25Verifier,
    nli: Option<NliVerifier>,
    config: CascadeConfig,
}

impl CascadeVerifier {
    /// Creates a BM25-only cascade verifier.
    #[must_use]
    pub fn bm25_only() -> Self {
        Self {
            bm25: Bm25Verifier::new(),
            nli: None,
            config: CascadeConfig::default(),
        }
    }

    /// Creates a cascade verifier with an optional NLI stage.
    #[must_use]
    pub fn new(config: CascadeConfig, nli: Option<NliVerifier>) -> Self {
        Self {
            bm25: Bm25Verifier::new(),
            nli,
            config,
        }
    }

    /// Verifies and fills all normalized citations.
    pub async fn verify_all(
        &self,
        answer_text: &str,
        sentence_offsets: &[(u32, u32)],
        citations: &[Citation],
        retrieved_chunks: &[ChunkRef],
    ) -> Vec<Citation> {
        if citations.is_empty() {
            return self
                .verify_uncited_answer(answer_text, sentence_offsets, retrieved_chunks)
                .await;
        }

        let chunks_by_id = retrieved_chunks
            .iter()
            .map(|chunk| (chunk.chunk_id, chunk))
            .collect::<BTreeMap<_, _>>();
        let mut out = Vec::with_capacity(citations.len());
        for citation in citations {
            let sentence = sentence_for(answer_text, sentence_offsets, citation.answer_span);
            let candidates = chunks_by_id
                .get(&citation.source_chunk_id)
                .map(|chunk| vec![(*chunk).clone()])
                .unwrap_or_else(|| retrieved_chunks.to_vec());
            let results = self
                .verify(VerificationInput {
                    answer_sentence: sentence,
                    candidate_chunks: &candidates,
                })
                .await;
            let mut citation = citation.clone();
            if let Some((_, verifier)) = results
                .into_iter()
                .find(|(chunk_id, _)| *chunk_id == citation.source_chunk_id)
            {
                citation.verifier = verifier;
            } else {
                citation.verifier = VerifierResult {
                    verified: false,
                    bm25_score: None,
                    nli_entailment: None,
                    nli_contradiction: None,
                    method: "bm25".to_string(),
                };
            }
            out.push(citation);
        }
        out
    }

    async fn verify_uncited_answer(
        &self,
        answer_text: &str,
        sentence_offsets: &[(u32, u32)],
        retrieved_chunks: &[ChunkRef],
    ) -> Vec<Citation> {
        let mut out = Vec::new();
        for (idx, _) in sentence_offsets.iter().enumerate() {
            let sentence = sentence_for(answer_text, sentence_offsets, idx as u32);
            let results = self
                .verify(VerificationInput {
                    answer_sentence: sentence,
                    candidate_chunks: retrieved_chunks,
                })
                .await;
            for (chunk_id, verifier) in results.into_iter().take(1) {
                if let Some(chunk) = retrieved_chunks
                    .iter()
                    .find(|chunk| chunk.chunk_id == chunk_id)
                {
                    out.push(Citation {
                        answer_span: idx as u32,
                        answer_span_bytes: None,
                        source_chunk_id: chunk.chunk_id,
                        source_node_uid: chunk.source_node_uid,
                        cited_text: Some(chunk.text.clone()),
                        vendor_score: None,
                        verifier,
                    });
                }
            }
        }
        out
    }
}

#[async_trait]
impl CitationVerifier for CascadeVerifier {
    async fn verify(&self, input: VerificationInput<'_>) -> Vec<(Uuid, VerifierResult)> {
        let bm25_hits = self
            .bm25
            .score(
                input.answer_sentence,
                input.candidate_chunks,
                self.config.bm25_top_k,
            )
            .await;

        if bm25_hits.len() < self.config.bm25_min_candidates {
            return bm25_hits
                .into_iter()
                .map(|(uid, score)| {
                    (
                        uid,
                        VerifierResult {
                            verified: false,
                            bm25_score: Some(score),
                            nli_entailment: None,
                            nli_contradiction: None,
                            method: "bm25".to_string(),
                        },
                    )
                })
                .collect();
        }

        let Some(nli) = &self.nli else {
            return bm25_hits
                .into_iter()
                .map(|(uid, score)| {
                    (
                        uid,
                        VerifierResult {
                            verified: score > 5.0,
                            bm25_score: Some(score),
                            nli_entailment: None,
                            nli_contradiction: None,
                            method: "bm25".to_string(),
                        },
                    )
                })
                .collect();
        };

        let shortlisted = input
            .candidate_chunks
            .iter()
            .filter(|chunk| bm25_hits.iter().any(|(uid, _)| *uid == chunk.chunk_id))
            .cloned()
            .collect::<Vec<_>>();
        let nli_results = nli
            .verify(VerificationInput {
                answer_sentence: input.answer_sentence,
                candidate_chunks: &shortlisted,
            })
            .await
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        bm25_hits
            .into_iter()
            .map(|(uid, bm25_score)| {
                let mut verifier = nli_results.get(&uid).cloned().unwrap_or(VerifierResult {
                    verified: false,
                    bm25_score: None,
                    nli_entailment: None,
                    nli_contradiction: None,
                    method: "bm25+nli".to_string(),
                });
                verifier.bm25_score = Some(bm25_score);
                verifier.verified = verifier.nli_entailment.unwrap_or(0.0)
                    >= self.config.nli_threshold
                    && verifier.nli_contradiction.unwrap_or(0.0) < 0.5;
                (uid, verifier)
            })
            .collect()
    }
}

fn sentence_for<'a>(answer_text: &'a str, offsets: &[(u32, u32)], idx: u32) -> &'a str {
    let Some((start, end)) = offsets.get(idx as usize).copied() else {
        return answer_text;
    };
    let start = start as usize;
    let end = end as usize;
    if start <= end && end <= answer_text.len() {
        &answer_text[start..end]
    } else {
        answer_text
    }
}
