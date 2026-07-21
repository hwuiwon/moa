# moa-providers

LLM, embedding, and rerank provider implementations for MOA, plus the runtime
registry, model catalog, and the routing, failover, and governance layers that
sit in front of vendor adapters.

## Structure

- `adapters/` — vendor-specific provider adapters: Anthropic, Gemini, OpenAI
  (Responses API), and the test-only scripted provider.
- `core/` — shared provider plumbing: provider factory, model catalog and
  pricing (`CATALOG`), `ModelRouter`, pacer, strict-schema helpers, and the
  coordination store.
- `embedding/` — embedding providers used by graph memory retrieval: Cohere,
  Gemini, OpenAI, ZeroEntropy, and a mock embedder.
- `rerank/` — rerankers applied after graph-memory candidate fusion: Cohere,
  ZeroEntropy, and a noop reranker.
- `failover.rs` — rate-limit-aware LLM failover across a configured model chain.
- `governance.rs` — provenance-aware DLP governance at the LLM egress boundary.
- `model_selection.rs` — shared helpers for `provider:model` selectors.
- `provider_policy.rs` — deployment-wide provider routing policy and endpoint
  capabilities.
- `registry.rs` — runtime registry for configured LLM provider families.
- `routing.rs` — provider-family and model-name routing descriptors.

## Features

- `mock-embedding` — deterministic `MockEmbedding` provider for tests.
- `scripted-provider` — `ScriptedProvider` LLM stub for offline harness tests.
- `test-util` — enables both of the above.
