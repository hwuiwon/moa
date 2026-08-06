//! Restate-side bridge for compiling one durable session turn request.

use std::{sync::Arc, time::Instant};

use moa_brain::{
    GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    lineage::emit_context_lineage, pipeline::history::HISTORY_SNAPSHOT_METADATA_KEY,
    query_rewrite::QueryRewriteResult,
};
use moa_core::{
    error::MoaError,
    error::Result,
    events::Event,
    events::EventType,
    session_engine::session_requires_processing,
    session_replay::record_pipeline_compile_duration,
    traits::Identity,
    traits::SessionStore,
    types::completion::CompletionRequest,
    types::context::{TURN_ID_METADATA_KEY, WorkingContext},
    types::events_stream::EventRange,
    types::events_stream::EventRecord,
    types::hands::SandboxFile,
    types::identifiers::SessionId,
    types::snapshot::ContextSnapshot,
};
use moa_lineage_citation::ChunkRef;
use moa_lineage_core::TurnId;
use moa_observability::{
    record_turn_pipeline_compile_duration, record_turn_snapshot_write_duration,
};
use moa_providers::ProviderRegistry;
use moa_security::inject_canary;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::connector_catalog::ScopedConnectorCatalogProvider;
use crate::services::llm_gateway::USER_TURN_METADATA_KEY;
use crate::tool_invocation::governed::TOOL_CATALOG_PIN_METADATA_KEY;

const TURN_EVENT_TAIL_LIMIT: usize = 32;
const QUERY_REWRITE_METADATA_KEY: &str = "query_rewrite";

/// Query-rewrite metadata cached for repeated compiles of one user message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct QueryRewriteCacheEntry {
    /// Sequence number of the user message this rewrite belongs to.
    pub user_sequence_num: u64,
    /// Query-rewrite result to reuse for later compile steps.
    pub result: QueryRewriteResult,
}

/// Prepared turn request outcome returned by the Restate-side bridge.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum PreparedTurnRequest {
    /// No new turn work is currently required.
    Idle,
    /// A compiled request is ready for `LLMGateway/complete`.
    Request(Box<CompletionRequest>),
}

/// Prepared turn request plus reusable preparation metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PreparedTurnRequestOutput {
    /// Request compilation outcome.
    pub prepared: PreparedTurnRequest,
    /// Active canary injected into this prepared request, when tools were available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_canary: Option<String>,
    /// Trusted files selected by the context pipeline for sandbox materialization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_sandbox_files: Vec<SandboxFile>,
    /// Query rewrite cache entry observed during compilation.
    pub query_rewrite_cache: Option<QueryRewriteCacheEntry>,
    /// Citable context chunks selected from the compiled provider window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citation_sources: Vec<ChunkRef>,
}

/// Explicit dependency owner for compiling durable root-turn requests.
#[derive(Clone)]
pub(crate) struct TurnRequestPreparer {
    session_store: Arc<dyn SessionStore>,
    config: Arc<moa_config::MoaConfig>,
    graph_pool: sqlx::PgPool,
    kms: Arc<dyn moa_crypto::KeyManagementProvider>,
    providers: Arc<ProviderRegistry>,
    connector_catalogs: ScopedConnectorCatalogProvider,
    graph_memory_retriever: Arc<moa_brain::pipeline::memory::GraphMemoryRetriever>,
    skill_injector: Arc<moa_brain::pipeline::skills::SkillInjector>,
    lineage: Arc<dyn moa_core::traits::LineageHandle>,
}

impl TurnRequestPreparer {
    /// Creates a request compiler from the runtime's shared dependencies.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_store: Arc<dyn SessionStore>,
        config: Arc<moa_config::MoaConfig>,
        graph_pool: sqlx::PgPool,
        kms: Arc<dyn moa_crypto::KeyManagementProvider>,
        providers: Arc<ProviderRegistry>,
        connector_catalogs: ScopedConnectorCatalogProvider,
        graph_memory_retriever: Arc<moa_brain::pipeline::memory::GraphMemoryRetriever>,
        skill_injector: Arc<moa_brain::pipeline::skills::SkillInjector>,
        lineage: Arc<dyn moa_core::traits::LineageHandle>,
    ) -> Self {
        Self {
            session_store,
            config,
            graph_pool,
            kms,
            providers,
            connector_catalogs,
            graph_memory_retriever,
            skill_injector,
            lineage,
        }
    }

    /// Compiles the next LLM request for a session from durable state.
    pub(crate) async fn prepare(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        identity: Identity,
        processing_required: bool,
        active_user_sequence_num: Option<u64>,
        cached_query_rewrite: Option<QueryRewriteCacheEntry>,
    ) -> Result<PreparedTurnRequestOutput> {
        let session_store = self.session_store.clone();
        let session = session_store.get_session(session_id).await?;
        if identity.tenant_id != session.tenant_id {
            return Err(MoaError::PermissionDenied(
                "turn identity tenant does not match the loaded session tenant".to_string(),
            ));
        }
        let recent_events = session_store
            .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
            .await?;
        let recent_events = preserve_active_user_event(
            session_store.as_ref(),
            session_id,
            recent_events,
            active_user_sequence_num,
        )
        .await?;
        // Session and Worker VOs admit system-triggered turns directly. A terminal
        // event from the turn that released that queued work can land after the
        // control event, so the generic tail heuristic must not veto the admitted
        // workflow. User-message turns still use the tail as their idle guard.
        if !processing_required && !session_requires_processing(&session, &recent_events) {
            return Ok(PreparedTurnRequestOutput {
                prepared: PreparedTurnRequest::Idle,
                active_canary: None,
                trusted_sandbox_files: Vec::new(),
                query_rewrite_cache: None,
                citation_sources: Vec::new(),
            });
        }

        let provider_registry = self.providers.clone();
        let config = self.config.clone();
        let capabilities =
            provider_registry.capabilities_for_model(Some(session.model.as_str()))?;
        let query_rewrite_provider = match provider_registry
            .resolve_rewriter_provider(&config.query_rewrite)
        {
            Ok(provider) => provider,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "failed to resolve query rewriter provider; continuing without query rewriting"
                );
                None
            }
        };
        let lineage = self.lineage.clone();
        // The root coordinator turn is sandbox-free: hard-exclude sandbox/compute
        // (hand-routed) tools so the coordinator never provisions a hand. Manifest-backed
        // `file_read` is kept so selected skill packages can be read without a sandbox.
        // The worker tool subsets (built from the unfiltered `tool_schemas`)
        // keep the hand tools, so all compute is delegated.
        let tool_catalog = self
            .connector_catalogs
            .for_session(&identity, &session)
            .await
            .map_err(|error| error.into_moa_error())?;
        let root_tool_schemas = coordinator_tool_schemas(tool_catalog.schemas().as_ref(), |name| {
            tool_catalog.snapshot().tool_requires_sandbox(name)
        });
        let tool_catalog_pin = tool_catalog.pin().clone();
        let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
            config.as_ref(),
            session_store.clone(),
            GraphMemoryPipelineOptions {
                graph_pool: self.graph_pool.clone(),
                kms: self.kms.clone(),
                shared_graph_memory_retriever: Some(self.graph_memory_retriever.clone()),
                retrieval_embedder: None,
                shared_skill_injector: Some(self.skill_injector.clone()),
                segment_store: None,
                compaction_llm_provider: None,
                query_rewrite_llm_provider: query_rewrite_provider,
                identity_prompt_override: None,
                tool_schemas: root_tool_schemas,
                lineage: lineage.clone(),
            },
        );
        let mut context = WorkingContext::new(&session, capabilities);
        context.set_caller_identity(identity);
        let active_user_turn = active_user_turn_text(&recent_events, active_user_sequence_num);
        context.set_recent_events(recent_events);
        if let Some(sequence_num) = active_user_sequence_num {
            context.insert_metadata("_moa.turn_seq", serde_json::json!(sequence_num));
        }
        if let Some(user_turn) = active_user_turn {
            // Memory ingestion reads this durable user-turn text instead of the
            // compiled messages, so injected reminders and replayed history never
            // re-enter fact extraction.
            context.insert_metadata(USER_TURN_METADATA_KEY, serde_json::json!(user_turn));
        }
        context.insert_metadata(
            TURN_ID_METADATA_KEY,
            serde_json::json!(turn_id.0.to_string()),
        );
        if let Some(agent_context) = context.agent_context.as_ref() {
            let policy = agent_context.parsed_policy_snapshot()?;
            policy.knowledge_policy.validate()?;
            if let Some(write_barrier) = policy.knowledge_policy.write_barrier {
                context.insert_metadata(
                    crate::services::llm_gateway::MEMORY_WRITE_BARRIER_METADATA_KEY,
                    serde_json::json!(write_barrier),
                );
            }
        }
        if let Some(cache) = cached_query_rewrite
            .filter(|cache| Some(cache.user_sequence_num) == active_user_sequence_num)
        {
            context.insert_metadata(
                QUERY_REWRITE_METADATA_KEY,
                serde_json::to_value(cache.result)?,
            );
        }
        let pipeline_span = tracing::info_span!("pipeline_compile");
        let compile_started = Instant::now();
        pipeline
            .run(&mut context)
            .instrument(pipeline_span.clone())
            .await?;
        let compile_duration = compile_started.elapsed();
        record_pipeline_compile_duration(compile_duration);
        record_turn_pipeline_compile_duration(compile_duration);
        let citation_sources = emit_context_lineage(
            lineage.as_ref(),
            turn_id,
            &session,
            &context,
            &pipeline_span,
        )
        .await;
        let active_canary = if context.tools().is_empty() {
            None
        } else {
            Some(inject_canary(&mut context))
        };
        persist_context_snapshot(
            session_store.as_ref(),
            &context,
            pipeline.snapshot_config().max_size_bytes,
        )
        .await;
        context.insert_metadata("_moa.session_id", serde_json::json!(session.id.to_string()));
        context.insert_metadata(
            "_moa.tenant_id",
            serde_json::json!(session.tenant_id.to_string()),
        );
        if let Some(contact) = session.contact.as_ref() {
            context.insert_metadata(
                "_moa.contact_id",
                serde_json::json!(contact.contact_id.to_string()),
            );
            context.insert_metadata(
                "_moa.contact.verification_state",
                serde_json::json!(contact.state.as_str()),
            );
            context.insert_metadata(
                "_moa.contact.verified",
                serde_json::json!(contact.state.is_verified()),
            );
        }
        context.insert_metadata("_moa.model", serde_json::json!(session.model.as_str()));
        context.insert_metadata(
            TOOL_CATALOG_PIN_METADATA_KEY,
            serde_json::to_value(tool_catalog_pin)?,
        );

        let query_rewrite_cache =
            query_rewrite_cache_from_context(active_user_sequence_num, &context);
        let trusted_sandbox_files = context.take_trusted_sandbox_files();
        Ok(PreparedTurnRequestOutput {
            prepared: PreparedTurnRequest::Request(Box::new(context.into_request())),
            active_canary,
            trusted_sandbox_files,
            query_rewrite_cache,
            citation_sources,
        })
    }
}

async fn preserve_active_user_event(
    session_store: &dyn SessionStore,
    session_id: SessionId,
    recent_events: Vec<EventRecord>,
    active_user_sequence_num: Option<u64>,
) -> Result<Vec<EventRecord>> {
    let Some(sequence_num) = active_user_sequence_num else {
        return Ok(recent_events);
    };
    if recent_events
        .iter()
        .any(|record| record.sequence_num == sequence_num)
    {
        return Ok(recent_events);
    }

    let anchor = session_store
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(sequence_num),
                to_seq: Some(sequence_num),
                event_types: Some(vec![EventType::UserMessage]),
                ..EventRange::default()
            },
        )
        .await?;
    Ok(merge_active_user_event(recent_events, anchor))
}

fn merge_active_user_event(
    mut recent_events: Vec<EventRecord>,
    anchor_events: Vec<EventRecord>,
) -> Vec<EventRecord> {
    recent_events.extend(anchor_events);
    recent_events.sort_by_key(|record| record.sequence_num);
    recent_events.dedup_by_key(|record| record.sequence_num);
    recent_events
}

/// Returns the active turn's durable user-message text from the merged event tail.
///
/// This is the only text memory ingestion may extract from: it is the user
/// event exactly as persisted, before any pipeline stage injects reminders,
/// digests, or planning hints into the compiled request.
fn active_user_turn_text(
    recent_events: &[EventRecord],
    active_user_sequence_num: Option<u64>,
) -> Option<String> {
    let sequence_num = active_user_sequence_num?;
    recent_events
        .iter()
        .find(|record| record.sequence_num == sequence_num)
        .and_then(|record| match &record.event {
            Event::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .filter(|text| !text.trim().is_empty())
}

/// Removes sandbox-requiring (hand-routed) tool schemas so the root coordinator
/// turn is offered only sandbox-free tools plus selected-skill `file_read`.
///
/// `requires_sandbox` classifies a tool *name* as hand/sandbox-executed; in
/// production this is [`moa_hands::ToolRouter::tool_requires_sandbox`], the
/// authoritative execution-routing predicate. Delegation, memory, retrieval, and
/// manifest-backed selected-skill reads are preserved. Schemas without a string
/// `name` are retained defensively: they cannot be classified as sandbox tools
/// and are never the hand-routed compute tools this filter targets. The input
/// slice is left untouched (a new `Vec` is returned), so the shared
/// `tool_schemas` source that worker subsets read stays complete.
pub(crate) fn coordinator_tool_schemas(
    schemas: &[serde_json::Value],
    requires_sandbox: impl Fn(&str) -> bool,
) -> Vec<serde_json::Value> {
    schemas
        .iter()
        .filter(|schema| {
            let Some(name) = schema.get("name").and_then(serde_json::Value::as_str) else {
                return true;
            };
            name == "file_read" || !requires_sandbox(name)
        })
        .cloned()
        .collect()
}

async fn persist_context_snapshot(
    session_store: &dyn SessionStore,
    context: &WorkingContext,
    max_size_bytes: usize,
) {
    let Some(snapshot_value) = context
        .metadata()
        .get(HISTORY_SNAPSHOT_METADATA_KEY)
        .cloned()
    else {
        return;
    };

    if snapshot_value.is_null() {
        let started_at = Instant::now();
        if let Err(error) = session_store.delete_snapshot(context.session_id).await {
            tracing::warn!(
                session_id = %context.session_id,
                error = %error,
                "compiled context snapshot delete failed in Restate bridge"
            );
            return;
        }

        record_turn_snapshot_write_duration(started_at.elapsed());
        return;
    }

    let snapshot = match serde_json::from_value::<ContextSnapshot>(snapshot_value) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                session_id = %context.session_id,
                error = %error,
                "failed to deserialize compiled context snapshot metadata in Restate bridge"
            );
            return;
        }
    };

    let serialized = match serde_json::to_vec(&snapshot) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(
                session_id = %context.session_id,
                error = %error,
                "failed to serialize compiled context snapshot in Restate bridge"
            );
            return;
        }
    };
    if serialized.len() > max_size_bytes {
        tracing::warn!(
            session_id = %context.session_id,
            snapshot_bytes = serialized.len(),
            max_snapshot_bytes = max_size_bytes,
            "compiled context snapshot exceeded expected size in Restate bridge"
        );
    }

    let started_at = Instant::now();
    if let Err(error) = session_store
        .put_snapshot(context.session_id, snapshot)
        .await
    {
        tracing::warn!(
            session_id = %context.session_id,
            error = %error,
            "compiled context snapshot persist failed in Restate bridge; next turn will fall back to replay"
        );
        return;
    }

    record_turn_snapshot_write_duration(started_at.elapsed());
}

fn query_rewrite_cache_from_context(
    active_user_sequence_num: Option<u64>,
    context: &WorkingContext,
) -> Option<QueryRewriteCacheEntry> {
    let user_sequence_num = active_user_sequence_num?;
    let result = context
        .metadata()
        .get(QUERY_REWRITE_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<QueryRewriteResult>(value.clone()).ok())?;
    Some(QueryRewriteCacheEntry {
        user_sequence_num,
        result,
    })
}

#[cfg(test)]
mod tests {
    use super::{coordinator_tool_schemas, merge_active_user_event};
    use chrono::Utc;
    use moa_core::{
        events::Event, events::EventType, types::events_stream::EventRecord,
        types::identifiers::SessionId,
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    /// Mirrors the production hand-routed (sandbox) tool catalog so the filter
    /// mechanics are exercised against representative names. The authoritative
    /// classification lives in `moa_hands::ToolRouter::tool_requires_sandbox`,
    /// pinned in `moa-hands` registration tests.
    fn requires_sandbox(name: &str) -> bool {
        matches!(
            name,
            "bash"
                | "grep"
                | "file_read"
                | "file_outline"
                | "file_search"
                | "file_write"
                | "str_replace"
        )
    }

    fn schema(name: &str) -> Value {
        json!({ "name": name, "description": name, "input_schema": {} })
    }

    fn names(schemas: &[Value]) -> Vec<String> {
        schemas
            .iter()
            .filter_map(|schema| schema.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }

    fn event_record(session_id: SessionId, sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num,
            event_type: EventType::from(&event),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    #[test]
    fn coordinator_tool_schemas_drops_hand_tools_but_keeps_delegation_and_read() {
        // Pins: the sandbox-free root coordinator loses hand-routed compute tools (bash,
        // file_write) while keeping manifest-backed skill reads, memory tools, and
        // delegation tools.
        let source = vec![
            schema("bash"),
            schema("file_read"),
            schema("file_write"),
            schema("session_search"),
            schema("tool_result_read"),
            schema("spawn_worker"),
            schema("wait_worker"),
        ];

        let coordinator = coordinator_tool_schemas(&source, requires_sandbox);

        let kept = names(&coordinator);
        assert!(!kept.contains(&"bash".to_string()), "bash must be excluded");
        assert!(
            !kept.contains(&"file_write".to_string()),
            "file_write must be excluded"
        );
        assert!(kept.contains(&"file_read".to_string()));
        assert!(kept.contains(&"session_search".to_string()));
        assert!(kept.contains(&"tool_result_read".to_string()));
        assert!(kept.contains(&"spawn_worker".to_string()));
        assert!(kept.contains(&"wait_worker".to_string()));
    }

    #[test]
    fn coordinator_filter_leaves_worker_tool_source_intact() {
        // Pins: filtering for the coordinator returns a new set and never mutates the shared
        // schema source, so a worker subset that allows "bash" still resolves it.
        let source = vec![schema("bash"), schema("session_search")];

        let coordinator = coordinator_tool_schemas(&source, requires_sandbox);
        assert!(!names(&coordinator).contains(&"bash".to_string()));

        // The worker path intersects its allow-list against the unfiltered source.
        let worker_subset = ["bash", "session_search"];
        let worker_tools = names(&source)
            .into_iter()
            .filter(|name| worker_subset.contains(&name.as_str()))
            .collect::<Vec<_>>();
        assert!(
            worker_tools.contains(&"bash".to_string()),
            "worker subset must still see the sandbox tool from the untouched source"
        );
    }

    #[test]
    fn coordinator_tool_schemas_retains_unnamed_schemas() {
        // Pins: a schema without a string name cannot be classified as a sandbox tool and is
        // preserved rather than silently dropped.
        let source = vec![json!({ "description": "anonymous" }), schema("bash")];

        let coordinator = coordinator_tool_schemas(&source, requires_sandbox);

        assert_eq!(coordinator.len(), 1);
        assert!(coordinator[0].get("name").is_none());
    }

    #[test]
    fn active_user_turn_text_reads_only_the_anchored_user_event() {
        // Pins: the ingestion user-turn metadata comes from the durable user event
        // for the active sequence number — never from another event type and never
        // when the turn has no user anchor (worker/internal requests).
        let session_id = SessionId::new();
        let events = vec![
            event_record(
                session_id,
                4,
                Event::UserMessage {
                    text: "old turn".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                session_id,
                7,
                Event::UserMessage {
                    text: "  set the deploy target to us-east-1  ".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];

        assert_eq!(
            super::active_user_turn_text(&events, Some(7)),
            Some("  set the deploy target to us-east-1  ".to_string())
        );
        assert_eq!(super::active_user_turn_text(&events, Some(5)), None);
        assert_eq!(super::active_user_turn_text(&events, None), None);
    }

    #[test]
    fn active_user_turn_text_skips_blank_user_events() {
        // Pins: attachment-only user events must not stamp an empty ingestion turn.
        let session_id = SessionId::new();
        let events = vec![event_record(
            session_id,
            2,
            Event::UserMessage {
                text: "   ".to_string(),
                attachments: Vec::new(),
            },
        )];

        assert_eq!(super::active_user_turn_text(&events, Some(2)), None);
    }

    #[test]
    fn active_user_anchor_is_merged_before_recent_tail() {
        // Pins: a long tool/worker turn can exceed the event-tail limit, but the current
        // user request must remain in the compiled context so final synthesis still has
        // the task objective.
        let session_id = SessionId::new();
        let recent = (1..=3)
            .map(|sequence_num| {
                event_record(
                    session_id,
                    sequence_num,
                    Event::BrainResponse {
                        text: format!("tool event {sequence_num}"),
                        thought_signature: None,
                        model: "test-model".into(),
                        model_tier: moa_core::types::provider::ModelTier::Main,
                        input_tokens_uncached: 1,
                        input_tokens_cache_write: 0,
                        input_tokens_cache_read: 0,
                        output_tokens: 1,
                        cost_cents: 0,
                        duration_ms: 1,
                        llm_ttft_ms: None,
                    },
                )
            })
            .collect::<Vec<_>>();
        let anchor = event_record(
            session_id,
            0,
            Event::UserMessage {
                text: "original task".to_string(),
                attachments: Vec::new(),
            },
        );

        let merged = merge_active_user_event(recent, vec![anchor]);

        assert_eq!(
            merged
                .iter()
                .map(|record| record.sequence_num)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(matches!(merged[0].event, Event::UserMessage { .. }));
    }
}
