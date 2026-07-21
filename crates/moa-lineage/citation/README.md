# moa-lineage-citation

Citation normalization and verification for lineage records. Vendor adapters
keep provider citation payloads as passthrough evidence and normalize only the
fields MOA lineage needs; the cascade verifier is model-agnostic and can run
with BM25-only scoring when no NLI model is configured.

## Structure

- `adapters` — provider-specific citation passthrough adapters
  (`AnthropicCitations`, `OpenAiAnnotations`, `CohereDocuments`,
  `VertexGrounding`) behind the `CitationAdapter` trait.
- `cascade` — two-stage `CascadeVerifier` that escalates from BM25 scoring
  to NLI verification.
- `verifiers` — citation verifier stages: `Bm25Verifier`, `NliVerifier`,
  and the shared `CitationVerifier` interface.
