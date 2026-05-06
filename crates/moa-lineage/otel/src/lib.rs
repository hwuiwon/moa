//! OpenTelemetry and OpenInference attribute emitters for lineage records.
//!
//! These helpers annotate the current span in parallel with the durable
//! lineage sink. They intentionally keep their surface to pure record-to-span
//! translation so storage and tracing remain independently versionable.

use moa_lineage_core::{ContextLineage, GenerationLineage, RetrievalLineage};
use opentelemetry::Value;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Emits OTel GenAI and OpenInference attributes for retrieval lineage.
pub fn emit_retrieval_attrs(span: &Span, record: &RetrievalLineage) {
    span.set_attribute("gen_ai.operation.name", Value::from("retrieval"));
    span.set_attribute(
        "gen_ai.data_source.id",
        Value::from(
            record
                .vector_hits
                .first()
                .map(|hit| hit.source.as_str())
                .unwrap_or("unknown")
                .to_string(),
        ),
    );
    span.set_attribute("openinference.span.kind", Value::from("RETRIEVER"));
    if let Ok(top_k) = serde_json::to_string(&record.top_k) {
        span.set_attribute("gen_ai.retrieval.documents", Value::from(top_k));
    }

    for (idx, hit) in record.vector_hits.iter().take(20).enumerate() {
        span.set_attribute(
            format!("retrieval.documents.{idx}.document.id"),
            Value::from(hit.chunk_id.to_string()),
        );
        span.set_attribute(
            format!("retrieval.documents.{idx}.document.score"),
            Value::from(hit.score as f64),
        );
        let metadata = serde_json::json!({
            "source": hit.source,
            "embedder": hit.embedder,
            "embed_dim": hit.embed_dim,
        });
        span.set_attribute(
            format!("retrieval.documents.{idx}.document.metadata"),
            Value::from(metadata.to_string()),
        );
    }

    if let Some(pgvector) = &record.introspection.pgvector {
        span.set_attribute(
            "moa.pgvector.ef_search",
            Value::from(i64::from(pgvector.ef_search)),
        );
        if let Some(buffers_hit) = pgvector.buffers_hit {
            span.set_attribute("moa.pgvector.buffers_hit", Value::from(buffers_hit as i64));
        }
        if let Some(buffers_read) = pgvector.buffers_read {
            span.set_attribute(
                "moa.pgvector.buffers_read",
                Value::from(buffers_read as i64),
            );
        }
    }
    if let Some(age) = &record.introspection.age {
        span.set_attribute(
            "moa.age.path_length",
            Value::from(i64::from(age.max_path_length)),
        );
        span.set_attribute(
            "moa.age.edges_walked",
            Value::from(i64::from(age.edges_walked)),
        );
        span.set_attribute(
            "moa.age.paths_returned",
            Value::from(i64::from(age.paths_returned)),
        );
    }
    if let Some(turbopuffer) = &record.introspection.turbopuffer {
        span.set_attribute(
            "moa.tpuf.namespace",
            Value::from(turbopuffer.namespace.clone()),
        );
        span.set_attribute(
            "moa.tpuf.consistency",
            Value::from(turbopuffer.consistency.clone()),
        );
        if let Some(billed_units) = turbopuffer.billed_units {
            span.set_attribute("moa.tpuf.billed_units", Value::from(billed_units));
        }
    }
    span.set_attribute(
        "moa.retrieval.total_ms",
        Value::from(i64::from(record.timings.total_ms)),
    );
}

/// Emits OTel GenAI and OpenInference attributes for compiled-context lineage.
pub fn emit_context_attrs(span: &Span, record: &ContextLineage) {
    span.set_attribute("gen_ai.operation.name", Value::from("context_compile"));
    span.set_attribute("openinference.span.kind", Value::from("CHAIN"));
    span.set_attribute(
        "moa.context.chunks_in_window",
        Value::from(record.chunks_in_window.len() as i64),
    );
    span.set_attribute(
        "moa.context.truncations",
        Value::from(record.truncations.len() as i64),
    );
    if let Some(tokens) = record.prefix_cache_hit_tokens {
        span.set_attribute(
            "gen_ai.usage.cache_read.input_tokens",
            Value::from(i64::from(tokens)),
        );
    }
    if let Some(tokens) = record.prefix_cache_miss_tokens {
        span.set_attribute(
            "gen_ai.usage.cache_creation.input_tokens",
            Value::from(i64::from(tokens)),
        );
    }
}

/// Emits OTel GenAI and OpenInference attributes for generation lineage.
pub fn emit_generation_attrs(span: &Span, record: &GenerationLineage) {
    span.set_attribute("gen_ai.operation.name", Value::from("chat"));
    span.set_attribute("openinference.span.kind", Value::from("LLM"));
    span.set_attribute("gen_ai.provider.name", Value::from(record.provider.clone()));
    span.set_attribute(
        "gen_ai.request.model",
        Value::from(record.request_model.clone()),
    );
    span.set_attribute(
        "gen_ai.response.model",
        Value::from(record.response_model.clone()),
    );
    span.set_attribute(
        "gen_ai.usage.input_tokens",
        Value::from(i64::from(record.usage.input_tokens)),
    );
    span.set_attribute(
        "gen_ai.usage.output_tokens",
        Value::from(i64::from(record.usage.output_tokens)),
    );
    if let Some(tokens) = record.usage.cache_read_tokens {
        span.set_attribute(
            "gen_ai.usage.cache_read.input_tokens",
            Value::from(i64::from(tokens)),
        );
    }
    if let Some(tokens) = record.usage.cache_creation_tokens {
        span.set_attribute(
            "gen_ai.usage.cache_creation.input_tokens",
            Value::from(i64::from(tokens)),
        );
    }
    span.set_attribute(
        "gen_ai.response.finish_reasons",
        Value::from(record.finish_reasons.join(",")),
    );
    span.set_attribute(
        "gen_ai.conversation.id",
        Value::from(record.session_id.to_string()),
    );
    span.set_attribute("moa.cost_micros", Value::from(record.cost_micros as i64));
}
