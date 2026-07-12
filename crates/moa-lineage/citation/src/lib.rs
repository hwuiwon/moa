//! Citation normalization and verification for lineage records.
//!
//! Vendor adapters keep provider citation payloads as passthrough evidence and
//! normalize only the fields needed by MOA lineage. The cascade verifier is
//! model-agnostic and can run with BM25-only scoring when an NLI model is not
//! configured.

mod adapters;
mod cascade;
mod verifiers;

pub use adapters::{
    AdapterError, AnthropicCitations, ChunkRef, CitationAdapter, CohereDocuments,
    OpenAiAnnotations, VertexGrounding,
};
pub use cascade::{CascadeConfig, CascadeVerifier};
pub use verifiers::{Bm25Verifier, CitationVerifier, NliVerifier, VerificationInput};
