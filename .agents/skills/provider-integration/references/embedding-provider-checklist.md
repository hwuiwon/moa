# Embedding Provider Checklist

For implementing `EmbeddingProvider` from `moa-core::traits::embedding`. Use this when adding an embedding or rerank service.

## Trait Surface

The trait lives at `crates/moa-core/src/traits/embedding.rs`. The two operations are batch embedding and (optionally) rerank.

Reference implementations to read first:

- OpenAI embeddings in `crates/moa-providers/src/embedding/openai.rs`
- Cohere v4 embed/rerank in `crates/moa-providers/src/embedding/cohere.rs` and `crates/moa-providers/src/rerank/cohere.rs`
- Mock embedding provider for tests

## Required Behaviors

1. **Batch input.** Embedding APIs charge per request as well as per token; batch as many inputs as the provider allows in one call. Default batch size: 100 inputs unless the provider documents a different cap.
2. **Dimension alignment.** The vector dimension must match the database column type. Current schema uses `halfvec(N)` for half-precision storage; the provider must produce the matching dimension at the appropriate precision. If the provider only outputs full-precision floats, downcast at the adapter boundary, not at the storage layer.
3. **Token usage reporting.** Like LLMs, embeddings need usage reporting for cost analytics. Populate `usage.input_tokens` even if the provider doesn't separate input from output for embeddings.
4. **Truncation policy.** When inputs exceed the provider's max sequence length, the adapter must either truncate (head, tail, or middle — pick a documented policy) or error. Silent truncation is not allowed; if you truncate, log it via `tracing` at `WARN`.
5. **Cancellation.** Same as LLMs; wrap the `reqwest` future in cancellation-aware select.

## Cohere v4 Specific Notes

Cohere v4 is the most idiosyncratic embedding API in the workspace today:

- It requires a separate `input_type` per call (`search_document`, `search_query`, `classification`, `clustering`). The adapter must accept this as a parameter, not hardcode it.
- It has separate models for embeddings vs rerank (`embed-english-v3.0` vs `rerank-english-v3.0`).
- Live tests are gated by `MOA_RUN_LIVE_COHERE_TESTS=1` and `COHERE_API_KEY` or `MOA_COHERE_API_KEY` (either name works; check the live test file for the current behavior).

If adding a new embedding provider, mirror Cohere's input-type plumbing as a precedent for providers that distinguish embedding intent.

## Database Coupling

Embedding providers couple to two layers in `moa-memory`:

1. **Vector storage** (`crates/moa-memory/vector/`): the storage layer expects a specific dimension. Adding a provider with a different dimension requires either a separate column or a migration.
2. **Hybrid retrieval**: the retrieval pipeline mixes vector search with graph traversal. A new embedding provider must be tested against the hybrid retrieval path, not just isolated embedding API calls.
3. **Provider factory** (`crates/moa-providers/src/embedding/factory.rs`): runtime construction and config routing live with provider implementations.

## Live Test Pattern

```rust
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_<NAME>_TESTS=1 and <NAME>_API_KEY"]
async fn name_live_embeds_short_corpus() {
    if std::env::var("MOA_RUN_LIVE_<NAME>_TESTS").as_deref() != Ok("1") {
        return;
    }
    // assert: dimension matches expected, batch returns one vec per input,
    //         usage.input_tokens is populated, vectors are not all-zero
}
```

Do not assert on specific vector values; embeddings are model-version-dependent and will drift.

## Wiring Points

1. The embedding-provider registry/factory in `moa-providers`.
2. The hybrid-retrieval pipeline in `moa-memory`.
3. The pricing table for cost analytics.

## Common Mistakes

- Producing 1536-dim vectors when the column is `halfvec(1024)` — silent truncation in Postgres, then mysterious recall regressions.
- Not handling the empty-input case; some providers return an error on `[]`, others return `[]`. The adapter normalizes to "return empty result without calling the API."
- Forgetting that rerank and embed are different surfaces; if the provider supports both, model them as two separate `EmbeddingProvider` constructors or two traits.
- Computing cosine similarity inside the adapter; that belongs in the retrieval layer.
