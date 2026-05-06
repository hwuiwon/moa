//! Citation verifier stages.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use moa_lineage_core::VerifierResult;
use uuid::Uuid;

use crate::adapters::ChunkRef;

/// Borrowed verifier input for one answer sentence.
#[derive(Clone, Copy)]
pub struct VerificationInput<'a> {
    /// Sentence or claim to verify.
    pub answer_sentence: &'a str,
    /// Candidate chunks to score.
    pub candidate_chunks: &'a [ChunkRef],
}

/// Verifies an answer sentence against candidate chunks.
#[async_trait]
pub trait CitationVerifier: Send + Sync {
    /// Returns one verifier result per selected source chunk.
    async fn verify(&self, input: VerificationInput<'_>) -> Vec<(Uuid, VerifierResult)>;
}

/// Lightweight BM25-style lexical verifier.
#[derive(Debug, Clone, Default)]
pub struct Bm25Verifier;

impl Bm25Verifier {
    /// Creates a BM25 verifier.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Scores candidate chunks by a small BM25-style formula.
    pub async fn score(
        &self,
        answer_sentence: &str,
        candidate_chunks: &[ChunkRef],
        top_k: usize,
    ) -> Vec<(Uuid, f32)> {
        score_bm25(answer_sentence, candidate_chunks, top_k)
    }
}

#[async_trait]
impl CitationVerifier for Bm25Verifier {
    async fn verify(&self, input: VerificationInput<'_>) -> Vec<(Uuid, VerifierResult)> {
        self.score(input.answer_sentence, input.candidate_chunks, usize::MAX)
            .await
            .into_iter()
            .map(|(chunk_id, score)| {
                (
                    chunk_id,
                    VerifierResult {
                        verified: score > 5.0,
                        bm25_score: Some(score),
                        nli_entailment: None,
                        nli_contradiction: None,
                        method: "bm25".to_string(),
                    },
                )
            })
            .collect()
    }
}

/// Optional NLI verifier facade.
///
/// HHEM-2.1-open is Apache-2.0 licensed. MOA does not commit ONNX binaries to
/// git; production deployments should download the model via an operational
/// step and construct this verifier with the loaded runtime. Until that runtime
/// is attached, this verifier uses a deterministic lexical entailment fallback
/// so the cascade remains testable in default CI.
#[derive(Clone)]
pub struct NliVerifier {
    model_name: Arc<str>,
}

impl NliVerifier {
    /// Creates a deterministic NLI verifier facade.
    #[must_use]
    pub fn new(model_name: impl Into<String>) -> Self {
        Self {
            model_name: Arc::from(model_name.into()),
        }
    }

    /// Returns the configured model name.
    #[must_use]
    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

#[async_trait]
impl CitationVerifier for NliVerifier {
    async fn verify(&self, input: VerificationInput<'_>) -> Vec<(Uuid, VerifierResult)> {
        let answer_tokens = tokenize(input.answer_sentence);
        input
            .candidate_chunks
            .iter()
            .map(|chunk| {
                let chunk_tokens = tokenize(&chunk.text);
                let overlap = if answer_tokens.is_empty() {
                    0.0
                } else {
                    answer_tokens.intersection(&chunk_tokens).count() as f32
                        / answer_tokens.len() as f32
                };
                let contradiction = contradiction_score(&answer_tokens, &chunk_tokens);
                (
                    chunk.chunk_id,
                    VerifierResult {
                        verified: overlap >= 0.5 && contradiction < 0.5,
                        bm25_score: None,
                        nli_entailment: Some(overlap),
                        nli_contradiction: Some(contradiction),
                        method: "bm25+nli".to_string(),
                    },
                )
            })
            .collect()
    }
}

fn score_bm25(answer_sentence: &str, chunks: &[ChunkRef], top_k: usize) -> Vec<(Uuid, f32)> {
    let query = tokenize(answer_sentence);
    if query.is_empty() || chunks.is_empty() {
        return Vec::new();
    }

    let mut document_frequencies = BTreeMap::<String, usize>::new();
    let documents = chunks
        .iter()
        .map(|chunk| {
            let tokens = tokenize_list(&chunk.text);
            let unique = tokens.iter().cloned().collect::<BTreeSet<_>>();
            for token in unique {
                *document_frequencies.entry(token).or_default() += 1;
            }
            tokens
        })
        .collect::<Vec<_>>();
    let avg_len =
        documents.iter().map(Vec::len).sum::<usize>().max(1) as f32 / documents.len().max(1) as f32;
    let n_docs = documents.len() as f32;
    let k1 = 1.2_f32;
    let b = 0.75_f32;

    let mut scores = documents
        .iter()
        .zip(chunks)
        .map(|(document, chunk)| {
            let mut tf = BTreeMap::<&str, usize>::new();
            for token in document {
                *tf.entry(token.as_str()).or_default() += 1;
            }
            let doc_len = document.len().max(1) as f32;
            let mut score = 0.0_f32;
            for token in &query {
                let Some(term_frequency) = tf.get(token.as_str()) else {
                    continue;
                };
                let df = *document_frequencies.get(token).unwrap_or(&1) as f32;
                let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
                let tf = *term_frequency as f32;
                score += idf * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * doc_len / avg_len));
            }
            (chunk.chunk_id, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect::<Vec<_>>();
    scores.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scores.truncate(top_k);
    scores
}

fn tokenize(input: &str) -> BTreeSet<String> {
    tokenize_list(input).into_iter().collect()
}

fn tokenize_list(input: &str) -> Vec<String> {
    input
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| part.len() > 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn contradiction_score(answer_tokens: &BTreeSet<String>, chunk_tokens: &BTreeSet<String>) -> f32 {
    let negations = ["not", "never", "no", "none", "without"];
    let answer_negated = negations.iter().any(|token| answer_tokens.contains(*token));
    let chunk_negated = negations.iter().any(|token| chunk_tokens.contains(*token));
    if answer_negated != chunk_negated {
        0.75
    } else {
        0.0
    }
}
