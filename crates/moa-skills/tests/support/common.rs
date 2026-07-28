//! Shared skill distillation/improvement integration-test fixtures.
//!
//! Consolidates the session-fixture, scripted-provider, and skill-seeding helpers
//! previously duplicated across the `distillation`, `draft_proposals`, `improver`,
//! and `regression` test binaries. Each binary uses only a subset of these helpers,
//! so the module allows dead code rather than warning per binary.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_config::MoaConfig;
use moa_core::{
    error::MoaError,
    error::Result as MoaResult,
    events::Event,
    traits::EmbeddingProvider,
    traits::LLMProvider,
    traits::SessionStore,
    types::action_policy::ActionRuleScope,
    types::agent::AgentContext,
    types::channel::Attachment,
    types::channel::Channel,
    types::completion::CompletionContent,
    types::completion::CompletionRequest,
    types::completion::CompletionResponse,
    types::completion::CompletionStream,
    types::completion::StopReason,
    types::completion::TokenUsage,
    types::contact::SessionActorRef,
    types::events_stream::EventRecord,
    types::experience::ExperienceRecord,
    types::experience::TaskFacetSet,
    types::experience::TaskFingerprint,
    types::identifiers::ModelId,
    types::identifiers::SegmentId,
    types::identifiers::SessionId,
    types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
    types::identifiers::ToolCallId,
    types::identifiers::UserId,
    types::model::ModelCapabilities,
    types::model::TokenPricing,
    types::model::ToolCallFormat,
    types::provider::ModelTier,
    types::segment_assessment::SegmentEvidence,
    types::segment_assessment::SegmentEvidenceKind,
    types::segment_assessment::SegmentEvidencePolarity,
    types::segment_assessment::SegmentOutcome,
    types::segments::{TaskSegment, deterministic_segment_id},
    types::session::SessionMeta,
    types::session::SessionStatus,
    types::tools::ToolOutput,
};
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;
use moa_skills::distiller::ExperienceDistillationInput;
use moa_skills::evidence::{EvidenceScope, SanitizedLearningEvidence, SegmentNarrative};
use moa_skills::format::{
    build_skill_path, parse_skill_markdown, render_skill_markdown, skill_metadata_from_document,
};
use moa_skills::registry::{NewSkill, Skill, SkillRegistry};
use moa_test_support::fixtures::tenant_id_from_storage_partition_id;
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

/// Successful session fixture with exactly five tool calls.
pub const SESSION_WITH_8_TOOL_CALLS: &str = include_str!("fixtures/session_with_8_tool_calls.json");
/// Successful session fixture below the distillation threshold.
pub const SESSION_WITH_4_TOOL_CALLS: &str = include_str!("fixtures/session_with_4_tool_calls.json");
/// Baseline skill fixture used by improvement and regression tests.
pub const BASELINE_SKILL: &str = include_str!("fixtures/baseline_skill.md");
/// Known-good improvement fixture returned by the scripted LLM.
pub const IMPROVED_SKILL: &str = include_str!("fixtures/improved_skill_diff.md");
/// Known-bad improvement fixture returned by the scripted LLM.
pub const REGRESSED_SKILL: &str = include_str!("fixtures/regressed_skill_diff.md");
/// Improvement fixture that renames the target skill; the improver must reject it.
pub const RENAMED_SKILL: &str = include_str!("fixtures/renamed_skill_diff.md");

#[derive(Debug, Deserialize)]
struct SessionFixture {
    session_id: Uuid,
    storage_partition_id: String,
    user_id: String,
    task: String,
    final_response: String,
    tool_calls: Vec<ToolCallFixture>,
}

#[derive(Debug, Deserialize)]
struct ToolCallFixture {
    tool_name: String,
    input: Value,
    output: String,
}

/// One parsed session fixture ready for distillation tests.
pub struct LoadedSession {
    /// Session metadata.
    pub session: SessionMeta,
    /// Event log records.
    pub events: Vec<EventRecord>,
}

/// Bootstraps an isolated Postgres test database, failing loudly when Postgres is unavailable.
///
/// These tests run only in the `db-memory` nextest lane. A missing database is a hard error
/// (panic) rather than a silent skip so the suite can never report a vacuous green without
/// exercising the real distillation/improvement path.
pub async fn setup_test_db() -> TestDb {
    bootstrap_test_db().await.expect(
        "bootstrap skills Postgres test database; start the compose Postgres or set MOA_DATABASE_URL",
    )
}

/// Builds an isolated config and memory directory for skill tests.
pub fn test_config(test_db: &TestDb) -> (MoaConfig, TempDir) {
    let temp_dir = tempfile::tempdir().expect("create skill test tempdir");
    let mut config = MoaConfig::default();
    config.database.url = test_db.database_url().to_string();
    config.local.memory_dir = temp_dir
        .path()
        .join("memory")
        .to_string_lossy()
        .into_owned();
    config.query_rewrite.enabled = false;
    (config, temp_dir)
}

/// Returns an owned learning store handle for the isolated test database.
pub fn learning_store(test_db: &TestDb) -> Arc<PostgresSessionStore> {
    Arc::new(test_db.store().clone())
}

/// Loads a session JSON fixture into typed session metadata and records.
pub fn load_session_fixture(json_text: &str) -> LoadedSession {
    let fixture: SessionFixture =
        serde_json::from_str(json_text).expect("parse skill session fixture");
    let storage_partition_id = StoragePartitionId::new(format!(
        "{}-{}",
        fixture.storage_partition_id,
        Uuid::now_v7().simple()
    ));
    let _user_id = fixture.user_id;
    let session = SessionMeta {
        id: SessionId(fixture.session_id),
        tenant_id: tenant_id_from_storage_partition_id(&storage_partition_id),
        title: Some(fixture.task.clone()),
        status: SessionStatus::Completed,
        channel: Channel::Chat,
        model: ModelId::new("scripted-skill-model"),
        ..SessionMeta::default()
    };
    let mut events = Vec::new();
    push_event(
        &mut events,
        session.id,
        Event::UserMessage {
            text: fixture.task,
            attachments: Vec::<Attachment>::new(),
        },
    );
    for tool_call in fixture.tool_calls {
        let tool_id = ToolCallId::new();
        push_event(
            &mut events,
            session.id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: tool_call.tool_name,
                input: tool_call.input,
                hand_id: None,
            },
        );
        push_event(
            &mut events,
            session.id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text(tool_call.output, Duration::from_millis(1)),
                original_output_tokens: None,
                success: true,
                duration_ms: 1,
                assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
            },
        );
    }
    push_event(
        &mut events,
        session.id,
        Event::BrainResponse {
            text: fixture.final_response,
            thought_signature: None,
            model: ModelId::new("scripted-skill-model"),
            model_tier: ModelTier::Auxiliary,
            input_tokens_uncached: 128,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 32,
            cost_cents: 0,
            duration_ms: 1,
            llm_ttft_ms: None,
        },
    );
    LoadedSession { session, events }
}

/// Returns a failed copy of a loaded session, preserving enough tool calls to pass the threshold.
pub fn failed_session(mut loaded: LoadedSession) -> LoadedSession {
    loaded.session.status = SessionStatus::Failed;
    let tool_id = ToolCallId::new();
    push_event(
        &mut loaded.events,
        loaded.session.id,
        Event::ToolError {
            tool_id,
            provider_tool_use_id: None,
            tool_name: "bash".to_string(),
            error: "final verification failed".to_string(),
            retryable: false,
        },
    );
    loaded
}

/// Builds a learnable experience-distillation input from a loaded session fixture.
///
/// The experience carries a resolved outcome above the learnability threshold and
/// reuses the fixture's events, so tests can drive the experience-native
/// distillation path without seeding segment-assessment rows.
pub async fn experience_input(
    loaded: &LoadedSession,
    task_summary: &str,
) -> ExperienceDistillationInput {
    let tools_used = loaded
        .events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let experience = ExperienceRecord {
        id: Uuid::now_v7(),
        segment_id: SegmentId::new(),
        session_id: loaded.session.id,
        tenant_id: loaded.session.tenant_id,
        user_id: UserId::new("fixture-user"),
        task_summary: Some(task_summary.to_string()),
        task_fingerprint: TaskFingerprint {
            hash: format!(
                "fixture-{}",
                task_summary.to_ascii_lowercase().replace(' ', "-")
            ),
            normalized_summary: task_summary.to_ascii_lowercase(),
            policy_version: "experience_v1".to_string(),
        },
        task_facets: TaskFacetSet::default(),
        actions: Vec::new(),
        resources: Vec::new(),
        outcome: SegmentOutcome::Resolved,
        confidence: 0.9,
        evidence: vec![SegmentEvidence {
            kind: SegmentEvidenceKind::Verification,
            polarity: SegmentEvidencePolarity::SupportsResolved,
            strength: 0.8,
            summary: "verification tool run passed".to_string(),
        }],
        tools_used,
        skills_activated: Vec::new(),
        skills_used: Vec::new(),
        turn_count: 2,
        token_cost: 10,
        duration_ms: Some(100),
        assessment_policy_version: "assessment_v1".to_string(),
        extraction_policy_version: "experience_v1".to_string(),
        created_at: moa_test_support::fixtures::pg_now(),
    };
    let evidence = sanitized_evidence(loaded, &experience).await;
    ExperienceDistillationInput {
        experience,
        attributions: Vec::new(),
        evidence,
    }
}

/// Monotonic segment index for seeded fixture segments.
static SEEDED_SEGMENT_INDEX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Builds a distillation input AND persists the rows its provenance points at.
///
/// A filed candidate carries typed `Session` and `Experience` sources with real
/// composite foreign keys, so a fixture whose session and experience exist only
/// in memory cannot commit one. That constraint is the guarantee — a candidate
/// nobody can attribute is a candidate no erasure can reach — so the fixture
/// seeds the rows production would already have written, rather than describing
/// a proposal that could not exist.
///
/// `experience_input` stays pure for the offline tests that have no database.
pub async fn seeded_experience_input(
    test_db: &TestDb,
    loaded: &LoadedSession,
    task_summary: &str,
) -> ExperienceDistillationInput {
    // Persist the fixture's events FIRST and build the input from the records the
    // store returned. A candidate's typed `Event` sources carry real composite
    // foreign keys, so evidence derived from in-memory events with invented ids
    // produces a candidate that cannot commit. Using the persisted records means
    // the ids in the provenance rows are the ids an erasure would walk.
    let persisted = seed_session_row(test_db, loaded).await;
    let loaded = &LoadedSession {
        session: loaded.session.clone(),
        events: persisted,
    };
    let input = experience_input(loaded, task_summary).await;
    // The experience hangs off a real segment, which hangs off a real session.
    // Seeding the whole chain rather than the one row that failed first is the
    // point: the chain IS the guarantee, and a fixture that satisfied only the
    // constraint it tripped over would keep drifting from the production shape.
    test_db
        .store()
        .create_segment(&TaskSegment {
            id: input.experience.segment_id,
            session_id: loaded.session.id,
            tenant_id: loaded.session.tenant_id.to_string(),
            // Unique per seeded segment: one fixture session can carry several
            // inputs, and `(session_id, segment_index)` is unique in the schema
            // because a session's segments really are ordered and distinct.
            segment_index: SEEDED_SEGMENT_INDEX.fetch_add(1, Ordering::Relaxed),
            task_summary: Some(task_summary.to_string()),
            started_at: input.experience.created_at,
            ended_at: Some(input.experience.created_at),
            turn_count: input.experience.turn_count,
            tools_used: input.experience.tools_used.clone(),
            skills_activated: input.experience.skills_activated.clone(),
            skills_used: input.experience.skills_used.clone(),
            token_cost: input.experience.token_cost,
            previous_segment_id: None,
            outcome: Some("resolved".to_string()),
            assessment: None,
            outcome_confidence: Some(input.experience.confidence),
        })
        .await
        .expect("seed the segment row the experience references");
    test_db
        .store()
        .append_experience_record(&input.experience)
        .await
        .expect("seed the experience row a candidate source references");
    input
}

/// Inserts the fixture's session and events, returning the persisted records.
///
/// Idempotent on the session so two inputs from the same fixture do not collide;
/// events are appended once per call because each input represents one segment's
/// worth of transcript.
pub async fn seed_session_row(test_db: &TestDb, loaded: &LoadedSession) -> Vec<EventRecord> {
    if moa_core::traits::SessionStore::get_session(test_db.store(), loaded.session.id)
        .await
        .is_err()
    {
        seed_session_meta(test_db, loaded).await;
    }
    let appends = loaded
        .events
        .iter()
        .map(|record| moa_session::EventAppend {
            event: record.event.clone(),
            dedupe_key: None,
        })
        .collect::<Vec<_>>();
    test_db
        .store()
        .append_events(loaded.session.id, appends)
        .await
        .expect("seed the events a candidate source references")
}

/// Creates the session row itself.
async fn seed_session_meta(test_db: &TestDb, loaded: &LoadedSession) {
    let mut meta = loaded.session.clone();
    // Session creation requires attribution, and so does everything downstream:
    // an unattributed session is exactly what the provenance chain cannot use.
    if meta.contact.is_none() && meta.created_by.is_none() {
        meta.created_by = Some(moa_core::types::contact::SessionActorRef::Identity {
            id: Uuid::from_u128(1),
        });
    }
    moa_core::traits::SessionStore::create_session(test_db.store(), meta)
        .await
        .expect("seed the session row a candidate source references");
}

/// Sanitizes a fixture session's events into learning evidence.
///
/// Uses the same deterministic heuristic classifier production uses, so a
/// fixture exercises the real sanitization gate rather than a permissive stand-in.
pub async fn sanitized_evidence(
    loaded: &LoadedSession,
    experience: &ExperienceRecord,
) -> SanitizedLearningEvidence {
    let assessment_summaries = experience
        .evidence
        .iter()
        .map(|evidence| evidence.summary.clone())
        .collect::<Vec<_>>();
    moa_skills::evidence::sanitize_segment_evidence(
        &moa_memory_pii::HeuristicPiiClassifier,
        EvidenceScope {
            tenant_id: loaded.session.tenant_id,
            contact_id: None,
            session_id: loaded.session.id,
            segment_id: experience.segment_id,
            experience_id: experience.id,
        },
        &loaded.events,
        SegmentNarrative {
            task_summary: experience.task_summary.as_deref(),
            assessment_summaries: &assessment_summaries,
        },
    )
    .await
    .expect("fixture transcripts sanitize cleanly")
}

/// Builds a model router backed by deterministic text responses.
pub fn scripted_router(responses: impl IntoIterator<Item = impl Into<String>>) -> Arc<ModelRouter> {
    let provider = TestProvider::new(responses.into_iter().map(Into::into).collect());
    Arc::new(ModelRouter::new(Arc::new(provider), None))
}

/// Returns a complete skill markdown document for ad hoc test skills.
pub fn skill_markdown(name: &str, description: &str, body: &str, version: &str) -> String {
    format!(
        "---\n\
         name: {name}\n\
         description: \"{description}\"\n\
         allowed-tools: bash file_search file_read\n\
         metadata:\n\
           moa-version: \"{version}\"\n\
           moa-tags: \"auth, regression\"\n\
           moa-estimated-tokens: \"300\"\n\
         ---\n\n\
         # {name}\n\n\
         {body}\n"
    )
}

/// Seeds one skill and returns its pipeline metadata.
pub async fn seed_skill(
    test_db: &TestDb,
    scope: ActionRuleScope,
    markdown: &str,
) -> moa_core::types::memory::SkillMetadata {
    let document = parse_skill_markdown(markdown).expect("parse seed skill");
    let rendered = render_skill_markdown(&document).expect("render seed skill");
    let registry = SkillRegistry::new(test_db.store().pool().clone());
    registry
        .upsert_by_name(NewSkill::from_skill_markdown(scope, rendered))
        .await
        .expect("seed skill");
    skill_metadata_from_document(build_skill_path(&document.frontmatter.name), &document)
}

/// Loads the active skill row by name when one exists.
pub async fn load_optional_active_skill(
    test_db: &TestDb,
    scope: &ActionRuleScope,
    skill_name: &str,
) -> Option<Skill> {
    SkillRegistry::new(test_db.store().pool().clone())
        .load_by_name(scope, skill_name)
        .await
        .expect("load optional active skill")
}

/// Returns the active semantic version parsed from the skill markdown.
pub async fn active_semantic_version(
    test_db: &TestDb,
    scope: &ActionRuleScope,
    skill_name: &str,
) -> String {
    let markdown = load_active_skill_markdown(test_db, scope, skill_name).await;
    parse_skill_markdown(&markdown)
        .expect("parse active skill")
        .frontmatter
        .version()
}

/// Counts skill artifact rows for one tenant skill name.
pub async fn skill_row_count(
    test_db: &TestDb,
    storage_partition_id: &StoragePartitionId,
    skill_name: &str,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) \
         FROM moa.artifact \
         WHERE storage_partition_id = $1 AND kind = 'skill' AND name = $2",
    )
    .bind(storage_partition_id.as_str())
    .bind(skill_name)
    .fetch_one(test_db.store().pool())
    .await
    .expect("count skill artifacts")
}

/// Counts artifact revisions for one tenant skill artifact.
pub async fn artifact_revision_count(
    test_db: &TestDb,
    storage_partition_id: &StoragePartitionId,
    skill_name: &str,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) \
         FROM moa.artifact a \
         JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid \
         WHERE a.storage_partition_id = $1 AND a.kind = 'skill' AND a.name = $2",
    )
    .bind(storage_partition_id.as_str())
    .bind(skill_name)
    .fetch_one(test_db.store().pool())
    .await
    .expect("count skill artifact revisions")
}

async fn load_active_skill_markdown(
    test_db: &TestDb,
    scope: &ActionRuleScope,
    skill_name: &str,
) -> String {
    let registry = SkillRegistry::new(test_db.store().pool().clone());
    let row = registry
        .load_by_name(scope, skill_name)
        .await
        .expect("load active skill")
        .expect("active skill exists");
    registry
        .load_skill_markdown(scope, row.skill_uid)
        .await
        .expect("load active skill markdown")
}

fn push_event(events: &mut Vec<EventRecord>, session_id: SessionId, event: Event) {
    events.push(EventRecord {
        id: Uuid::now_v7(),
        session_id,
        sequence_num: events.len() as u64 + 1,
        event_type: event.event_type(),
        event,
        timestamp: moa_test_support::fixtures::pg_now(),
        brain_id: None,
        hand_id: None,
        token_count: None,
    });
}

struct TestProvider {
    responses: Mutex<VecDeque<String>>,
}

impl TestProvider {
    fn new(responses: VecDeque<String>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LLMProvider for TestProvider {
    fn name(&self) -> &str {
        "skill-test"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("scripted-skill-model"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        let text = self
            .responses
            .lock()
            .map_err(|error| MoaError::ProviderError(format!("test provider poisoned: {error}")))?
            .pop_front()
            .ok_or_else(|| {
                MoaError::ProviderError("skill test provider ran out of responses".to_string())
            })?;
        Ok(text_completion_stream(text))
    }
}

/// Builds a single-shot text completion stream for a scripted provider response.
fn text_completion_stream(text: String) -> CompletionStream {
    let output_tokens = text.chars().count().div_ceil(4);
    CompletionStream::from_response(CompletionResponse {
        text: text.clone(),
        content: vec![CompletionContent::Text(text)],
        stop_reason: StopReason::EndTurn,
        model: ModelId::new("scripted-skill-model"),
        usage: TokenUsage {
            input_tokens_uncached: 32,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens,
        },
        duration_ms: 1,
        thought_signature: None,
    })
}

/// A model provider that simulates a rival generalization pass landing while this
/// pass's model call is in flight.
///
/// On its first `complete`, it rewrites the target candidate's
/// `draft_artifact_revision_uid` to a fresh value — exactly what a concurrent pass
/// that rewrote the draft would do — before returning `response`. Later calls just
/// return `response`. This drives the optimistic-concurrency retry deterministically:
/// the first apply sees the changed revision and retries, and the retry lands
/// cleanly against the rival's draft instead of clobbering it.
struct RaceMutatingProvider {
    store: Arc<PostgresSessionStore>,
    tenant_id: TenantId,
    candidate_id: Uuid,
    response: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LLMProvider for RaceMutatingProvider {
    fn name(&self) -> &str {
        "race-mutating-skill"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("scripted-skill-model"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // A rival pass advances the draft revision while this model call runs.
            if let Some(mut candidate) = self
                .store
                .get_learning_candidate(&self.tenant_id, self.candidate_id)
                .await?
            {
                if let Some(object) = candidate.payload.as_object_mut() {
                    object.insert(
                        "draft_artifact_revision_uid".to_string(),
                        Value::String(Uuid::now_v7().to_string()),
                    );
                }
                candidate.updated_at = moa_test_support::fixtures::pg_now();
                self.store.append_learning_candidate(&candidate).await?;
            }
        }
        Ok(text_completion_stream(self.response.clone()))
    }
}

/// Builds a router whose provider rewrites the candidate's draft revision on its
/// first call, plus the call counter, for the optimistic-concurrency retry test.
pub fn race_mutating_router(
    store: Arc<PostgresSessionStore>,
    tenant_id: TenantId,
    candidate_id: Uuid,
    response: impl Into<String>,
) -> (Arc<ModelRouter>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = RaceMutatingProvider {
        store,
        tenant_id,
        candidate_id,
        response: response.into(),
        calls: calls.clone(),
    };
    (Arc::new(ModelRouter::new(Arc::new(provider), None)), calls)
}

/// Returns a tenant artifact-visibility scope for tests.
pub fn tenant_scope(storage_partition_id: &StoragePartitionId) -> ActionRuleScope {
    ActionRuleScope::Tenant {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
    }
}

/// Returns the tenant storage key for session-scoped learning rows.
pub fn session_storage_partition_id(session: &SessionMeta) -> StoragePartitionId {
    StoragePartitionId::for_tenant(session.tenant_id)
}

/// Dimensionality of the learning vector space; must match `halfvec(1024)`.
const LEARNING_EMBEDDING_DIM: usize = 1024;

/// A fixed unit probe vector every scripted embedding maps to.
///
/// Because every input embeds to the same vector, a probe and a stored embedding
/// are always at cosine distance `0` — the maximally-similar case — which lets a
/// semantic test drive dedup/routing deterministically without a real model.
pub fn learning_probe_vector() -> Vec<f32> {
    let mut vector = vec![0.0_f32; LEARNING_EMBEDDING_DIM];
    vector[0] = 1.0;
    vector
}

/// A deterministic embedding provider that maps every input to one fixed 1024-dim
/// vector and counts the inputs it was asked to embed.
///
/// The call counter lets a test assert that the semantic layer either ran (the
/// probe was embedded) or was skipped entirely (zero embeds on the lexical path).
pub struct ScriptedEmbedder {
    vector: Vec<f32>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbeddingProvider for ScriptedEmbedder {
    fn model_id(&self) -> &str {
        "scripted-embed"
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    async fn embed(&self, inputs: &[String]) -> MoaResult<Vec<Vec<f32>>> {
        self.calls.fetch_add(inputs.len(), Ordering::SeqCst);
        Ok(inputs.iter().map(|_| self.vector.clone()).collect())
    }
}

/// Builds a scripted embedder and the shared counter of inputs it embeds.
pub fn scripted_embedder() -> (Arc<dyn EmbeddingProvider>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let embedder = Arc::new(ScriptedEmbedder {
        vector: learning_probe_vector(),
        calls: calls.clone(),
    });
    (embedder, calls)
}

fn embedding_session_meta(tenant_id: TenantId) -> SessionMeta {
    SessionMeta {
        tenant_id,
        created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
        model: ModelId::new("scripted-skill-model"),
        agent_context: Some(AgentContext::system_default()),
        ..SessionMeta::default()
    }
}

/// Persists a real session + segment + assessed experience with a task embedding.
///
/// Semantic tests need experience rows whose `task_embedding` the nearest-neighbor
/// queries can find, which requires the full FK chain (session, segment,
/// experience) plus a set embedding. The experience id is supplied by the caller
/// so a test can align it with the source-experience id an open proposal already
/// references. Returns nothing; the caller already holds the id.
pub async fn seed_embedded_experience(
    test_db: &TestDb,
    experience_id: Uuid,
    tenant_id: TenantId,
    fingerprint_hash: &str,
    task_summary: &str,
    embedding: &[f32],
    created_at: DateTime<Utc>,
) {
    let store = test_db.store();
    // The experience may already exist: `seeded_experience_input` persists the
    // rows a candidate's provenance points at, and a test that then wants that
    // same experience discoverable by the dedup nearest-neighbour is asking for
    // an EMBEDDING, not a second row. Re-inserting would violate the primary key
    // and, worse, a fixture that "fixed" it by minting a new id would embed a
    // different experience than the one the proposal was filed from.
    let already_persisted: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM experience_records WHERE id = $1)")
            .bind(experience_id)
            .fetch_one(store.pool())
            .await
            .expect("probe for an already-seeded experience");
    if already_persisted {
        store
            .set_experience_task_embeddings(
                &[(experience_id, task_summary.to_string(), embedding.to_vec())],
                "scripted-embed",
                1,
            )
            .await
            .expect("set experience embedding");
        return;
    }
    let session_id: SessionId = store
        .create_session(embedding_session_meta(tenant_id))
        .await
        .expect("create embedded-experience session");
    let segment_id = deterministic_segment_id(session_id, 0);
    let tools_used = vec!["bash".to_string(), "file_read".to_string()];
    store
        .create_segment(&TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: tenant_id.to_string(),
            segment_index: 0,
            task_summary: Some(task_summary.to_string()),
            started_at: created_at,
            ended_at: Some(created_at),
            turn_count: 1,
            tools_used: tools_used.clone(),
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 0,
            previous_segment_id: None,
            outcome: Some(SegmentOutcome::Resolved.as_str().to_string()),
            assessment: None,
            outcome_confidence: Some(0.9),
        })
        .await
        .expect("create embedded-experience segment");
    let experience = ExperienceRecord {
        id: experience_id,
        segment_id,
        session_id,
        tenant_id,
        user_id: UserId::new("fixture-user"),
        task_summary: Some(task_summary.to_string()),
        task_fingerprint: TaskFingerprint {
            hash: fingerprint_hash.to_string(),
            normalized_summary: task_summary.to_ascii_lowercase(),
            policy_version: "experience_v1".to_string(),
        },
        task_facets: TaskFacetSet::default(),
        actions: Vec::new(),
        resources: Vec::new(),
        outcome: SegmentOutcome::Resolved,
        confidence: 0.9,
        evidence: Vec::new(),
        tools_used,
        skills_activated: Vec::new(),
        skills_used: Vec::new(),
        turn_count: 1,
        token_cost: 0,
        duration_ms: None,
        assessment_policy_version: "assessment_v1".to_string(),
        extraction_policy_version: "experience_v1".to_string(),
        created_at,
    };
    store
        .append_experience_record(&experience)
        .await
        .expect("persist embedded experience");
    store
        .set_experience_task_embeddings(
            &[(experience_id, task_summary.to_string(), embedding.to_vec())],
            "scripted-embed",
            1,
        )
        .await
        .expect("set experience embedding");
}
