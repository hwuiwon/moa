//! Consolidated offline provider integration tests (one harness binary per lane).

#[path = "support"]
mod support {
    pub mod anthropic_wiremock;
    pub mod gemini_wiremock;
    pub mod openai_wiremock;
    pub mod wiremock_common;
}

#[path = "providers_offline/anthropic_offline.rs"]
mod anthropic_offline;
#[path = "providers_offline/anthropic_provider.rs"]
mod anthropic_provider;
#[path = "providers_offline/cache_control_markers.rs"]
mod cache_control_markers;
#[path = "providers_offline/cohere_embedding_offline.rs"]
mod cohere_embedding_offline;
#[path = "providers_offline/cohere_reranker_offline.rs"]
mod cohere_reranker_offline;
#[path = "providers_offline/gemini_embedding_offline.rs"]
mod gemini_embedding_offline;
#[path = "providers_offline/gemini_offline.rs"]
mod gemini_offline;
#[path = "providers_offline/openai_embedding_offline.rs"]
mod openai_embedding_offline;
#[path = "providers_offline/openai_offline.rs"]
mod openai_offline;
#[path = "providers_offline/openai_provider.rs"]
mod openai_provider;
#[path = "providers_offline/provider_coordination_offline.rs"]
mod provider_coordination_offline;
#[path = "providers_offline/zeroentropy_embedding_offline.rs"]
mod zeroentropy_embedding_offline;
#[path = "providers_offline/zeroentropy_reranker_offline.rs"]
mod zeroentropy_reranker_offline;
