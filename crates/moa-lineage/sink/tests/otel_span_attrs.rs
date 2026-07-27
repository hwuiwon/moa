//! Span-attribute snapshot tests for the lineage OTel emitters.
//!
//! These tests install a `tracing-opentelemetry` layer backed by an in-memory
//! OpenTelemetry span exporter, run one emitter against a span, then read the
//! exported `SpanData` attributes back. This pins the GenAI/OpenInference
//! attribute contract, the 20-document cap on retrieval, and the
//! `unwrap_or("unknown")` data-source fallback.

use std::collections::HashMap;
use std::time::Duration;

use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::UserId,
};
use moa_lineage_core::{
    BackendIntrospection, ContextChunk, ContextLineage, GenerationLineage, GenerationTokenUsage,
    GraphIntrospection, PgvectorIntrospection, RetrievalLineage, RetrievalStage, StageTimings,
    TruncationEvent, TurbopufferIntrospection, TurnId, VecHit,
};
use moa_lineage_sink::otel::{emit_context_attrs, emit_generation_attrs, emit_retrieval_attrs};
use moa_memory_types::MemoryScope;
use opentelemetry::Value;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{
    InMemorySpanExporterBuilder, SdkTracerProvider, SimpleSpanProcessor,
};
use tracing_subscriber::prelude::*;
use uuid::Uuid;

/// Runs `emit` against a fresh span under an in-memory OTel exporter and returns
/// the exported span's attributes keyed by attribute name.
fn capture_span_attributes(emit: impl FnOnce(&tracing::Span)) -> HashMap<String, Value> {
    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let tracer = provider.tracer("moa-lineage-sink-otel-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("lineage");
        emit(&span);
        // Span closes on drop here, ending the OTel span and flushing it through
        // the simple processor.
    });
    provider.force_flush().expect("span provider should flush");

    let spans = exporter
        .get_finished_spans()
        .expect("in-memory exporter should return finished spans");
    assert_eq!(spans.len(), 1, "expected exactly one exported span");

    spans[0]
        .attributes
        .iter()
        .map(|kv| (kv.key.as_str().to_string(), kv.value.clone()))
        .collect()
}

fn assert_attr(attrs: &HashMap<String, Value>, key: &str, expected: Value) {
    match attrs.get(key) {
        Some(value) => assert_eq!(*value, expected, "attribute `{key}` value mismatch"),
        None => panic!(
            "missing attribute `{key}`; present keys: {:?}",
            attrs.keys().collect::<Vec<_>>()
        ),
    }
}

fn vec_hit(idx: u128, source: &str) -> VecHit {
    VecHit {
        chunk_id: Uuid::from_u128(idx + 1),
        score: 0.5,
        source: source.to_string(),
        embedder: "text-embedding-3-small".to_string(),
        embed_dim: 1536,
    }
}

fn retrieval_record(
    vector_hits: Vec<VecHit>,
    introspection: BackendIntrospection,
) -> RetrievalLineage {
    RetrievalLineage {
        turn_id: TurnId::new_v7(),
        session_id: SessionId::new(),
        storage_partition_id: StoragePartitionId::new("tenant-otel"),
        user_id: UserId::new("user-otel"),
        scope: MemoryScope::Tenant {
            tenant_id: TenantId::from(Uuid::from_u128(0x42)),
        },
        ts: chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
            chrono::Utc::now().timestamp_micros(),
        )
        .expect("microsecond timestamp"),
        query_original: "what is oauth".to_string(),
        query_expansions: Vec::new(),
        vector_hits,
        graph_paths: Vec::new(),
        fusion_scores: Vec::new(),
        rerank_scores: Vec::new(),
        top_k: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
        searched_scopes: Vec::new(),
        selected_hits: Vec::new(),
        filters: serde_json::Value::Null,
        timings: StageTimings {
            total_ms: 1234,
            ..StageTimings::default()
        },
        introspection,
        stage: RetrievalStage::Single,
    }
}

#[test]
fn emit_retrieval_attrs_snapshots_core_attributes_and_caps_documents_at_20() {
    // 25 hits, first source "pgvector": the emitter caps per-document attributes
    // at 20 and reports the first hit's source as the data source.
    let hits = (0..25)
        .map(|idx| vec_hit(idx, "pgvector"))
        .collect::<Vec<_>>();
    let introspection = BackendIntrospection {
        pgvector: Some(PgvectorIntrospection {
            ef_search: 64,
            iterative_scan: None,
            buffers_hit: Some(10),
            buffers_read: Some(3),
            planning_ms: None,
            execution_ms: None,
        }),
        graph: Some(GraphIntrospection {
            max_path_length: 4,
            edges_walked: 12,
            paths_returned: 5,
        }),
        turbopuffer: Some(TurbopufferIntrospection {
            namespace: "ns-otel".to_string(),
            consistency: "strong".to_string(),
            billed_units: Some(7.0),
            client_wall_clock_ms: 8,
        }),
    };
    let record = retrieval_record(hits, introspection);

    let attrs = capture_span_attributes(|span| emit_retrieval_attrs(span, &record));

    assert_attr(&attrs, "gen_ai.operation.name", Value::from("retrieval"));
    assert_attr(&attrs, "openinference.span.kind", Value::from("RETRIEVER"));
    assert_attr(&attrs, "gen_ai.data_source.id", Value::from("pgvector"));
    assert_attr(&attrs, "moa.retrieval.total_ms", Value::from(1234_i64));

    // Introspection branches.
    assert_attr(&attrs, "moa.pgvector.ef_search", Value::from(64_i64));
    assert_attr(&attrs, "moa.pgvector.buffers_hit", Value::from(10_i64));
    assert_attr(&attrs, "moa.graph.path_length", Value::from(4_i64));
    assert_attr(&attrs, "moa.graph.edges_walked", Value::from(12_i64));
    assert_attr(&attrs, "moa.tpuf.namespace", Value::from("ns-otel"));
    assert_attr(&attrs, "moa.tpuf.consistency", Value::from("strong"));

    // 20-document cap: index 19 present, index 20 absent.
    assert!(
        attrs.contains_key("retrieval.documents.19.document.id"),
        "the 20th document (index 19) should be emitted"
    );
    assert!(
        !attrs.contains_key("retrieval.documents.20.document.id"),
        "documents beyond the 20-item cap must not be emitted"
    );
}

#[test]
fn emit_retrieval_attrs_uses_unknown_data_source_when_no_vector_hits() {
    let record = retrieval_record(Vec::new(), BackendIntrospection::default());

    let attrs = capture_span_attributes(|span| emit_retrieval_attrs(span, &record));

    assert_attr(&attrs, "gen_ai.data_source.id", Value::from("unknown"));
    assert!(
        !attrs.contains_key("retrieval.documents.0.document.id"),
        "no documents should be emitted when there are no vector hits"
    );
    assert!(
        !attrs.contains_key("moa.pgvector.ef_search"),
        "absent introspection must not emit backend attributes"
    );
}

#[test]
fn emit_context_attrs_snapshots_window_and_cache_attributes() {
    let context_chunk = |position: u16| ContextChunk {
        chunk_id: Uuid::from_u128(u128::from(position) + 1),
        source_uid: Uuid::from_u128(u128::from(position) + 100),
        position,
        estimated_tokens: 32,
        role: "context".to_string(),
        source_refs: Vec::new(),
    };
    let record = ContextLineage {
        turn_id: TurnId::new_v7(),
        session_id: SessionId::new(),
        storage_partition_id: StoragePartitionId::new("tenant-otel"),
        user_id: UserId::new("user-otel"),
        ts: chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
            chrono::Utc::now().timestamp_micros(),
        )
        .expect("microsecond timestamp"),
        chunks_in_window: vec![context_chunk(0), context_chunk(1), context_chunk(2)],
        truncations: vec![
            TruncationEvent {
                chunk_id: Some(Uuid::from_u128(9)),
                reason: "budget".to_string(),
                tokens_dropped: 12,
            },
            TruncationEvent {
                chunk_id: None,
                reason: "dedupe".to_string(),
                tokens_dropped: 4,
            },
        ],
        prefix_cache_hit_tokens: Some(100),
        prefix_cache_miss_tokens: Some(50),
        total_input_tokens_estimated: 999,
    };

    let attrs = capture_span_attributes(|span| emit_context_attrs(span, &record));

    assert_attr(&attrs, "moa.operation.name", Value::from("context_compile"));
    assert_attr(&attrs, "openinference.span.kind", Value::from("CHAIN"));
    assert_attr(&attrs, "moa.context.chunks_in_window", Value::from(3_i64));
    assert_attr(&attrs, "moa.context.truncations", Value::from(2_i64));
    assert_attr(
        &attrs,
        "gen_ai.usage.cache_read.input_tokens",
        Value::from(100_i64),
    );
    assert_attr(
        &attrs,
        "gen_ai.usage.cache_creation.input_tokens",
        Value::from(50_i64),
    );
}

#[test]
fn emit_generation_attrs_snapshots_model_usage_and_cost() {
    let record = GenerationLineage {
        turn_id: TurnId::new_v7(),
        session_id: SessionId::new(),
        storage_partition_id: StoragePartitionId::new("tenant-otel"),
        user_id: UserId::new("user-otel"),
        ts: chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
            chrono::Utc::now().timestamp_micros(),
        )
        .expect("microsecond timestamp"),
        provider: "anthropic".to_string(),
        request_model: "claude-sonnet".to_string(),
        response_model: "claude-sonnet-20260101".to_string(),
        usage: GenerationTokenUsage {
            input_tokens: 200,
            output_tokens: 80,
            cache_read_tokens: Some(40),
            cache_creation_tokens: Some(10),
        },
        finish_reasons: vec!["stop".to_string(), "end_turn".to_string()],
        tool_calls: Vec::new(),
        cost_micros: 4321,
        duration: Duration::from_millis(500),
        trace_id: None,
        span_id: None,
        response_event_id: None,
        response_event_sequence_num: None,
    };
    let session_id = record.session_id.to_string();

    let attrs = capture_span_attributes(|span| emit_generation_attrs(span, &record));

    assert_attr(&attrs, "gen_ai.operation.name", Value::from("chat"));
    assert_attr(&attrs, "openinference.span.kind", Value::from("LLM"));
    assert_attr(&attrs, "gen_ai.provider.name", Value::from("anthropic"));
    assert_attr(&attrs, "gen_ai.request.model", Value::from("claude-sonnet"));
    assert_attr(
        &attrs,
        "gen_ai.response.model",
        Value::from("claude-sonnet-20260101"),
    );
    assert_attr(&attrs, "gen_ai.usage.input_tokens", Value::from(200_i64));
    assert_attr(&attrs, "gen_ai.usage.output_tokens", Value::from(80_i64));
    assert_attr(
        &attrs,
        "gen_ai.usage.cache_read.input_tokens",
        Value::from(40_i64),
    );
    assert_attr(
        &attrs,
        "gen_ai.usage.cache_creation.input_tokens",
        Value::from(10_i64),
    );
    assert_attr(
        &attrs,
        "gen_ai.response.finish_reasons",
        Value::from("stop,end_turn"),
    );
    assert_attr(&attrs, "gen_ai.conversation.id", Value::from(session_id));
    assert_attr(&attrs, "moa.cost_micros", Value::from(4321_i64));
}
