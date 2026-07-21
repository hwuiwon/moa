//! PostgreSQL-backed `PostgresSessionStore` integration coverage.

use chrono::Utc;
use moa_core::traits::{SessionChannelBindingUpdate, SessionChannelStore};
use moa_core::{
    error::MoaError, events::Event, events::EventType, traits::SessionStore,
    types::agent::AgentContext, types::channel::ChannelRef, types::contact::ContactId,
    types::contact::ContactRef, types::contact::SessionActorRef,
    types::experience::AttributionEffect, types::experience::AttributionKind,
    types::experience::AttributionSubjectType, types::experience::ExperienceAttribution,
    types::experience::ExperienceRecord, types::experience::LearningCandidate,
    types::experience::LearningCandidateStatus, types::experience::LearningCandidateStatusUpdate,
    types::experience::LearningCandidateType, types::experience::LearningRiskClass,
    types::experience::TaskFacetSet, types::experience::TaskFingerprint,
    types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::identifiers::ToolCallId, types::identifiers::UserId, types::learning::LearningEntry,
    types::provider::ModelTier, types::segment_assessment::AssessmentPhase,
    types::segment_assessment::SegmentAssessment, types::segment_assessment::SegmentEvidence,
    types::segment_assessment::SegmentEvidenceKind,
    types::segment_assessment::SegmentEvidencePolarity, types::segment_assessment::SegmentOutcome,
    types::segments::SegmentCompletion, types::segments::TaskSegment,
    types::segments::deterministic_segment_id, types::session::SessionMeta,
    types::snapshot::ContextSnapshot, types::snapshot::FileReadDedupState,
    types::tools::ToolOutput,
};
use moa_session::{
    EventAppend, PostgresSessionStore,
    store::{
        DashboardEventCursor, DashboardEventPageRequest, DashboardSessionListCursor,
        DashboardSessionListRequest,
    },
    testing,
};
use moa_test_support::postgres::{
    test_action_policy_rules, test_create_and_get_session, test_emit_and_get_events,
    test_event_search, test_list_sessions_with_filter, test_session_status_update,
    test_tenant_cost_since,
};
use sqlx::{PgPool, Row};
use std::{future::Future, time::Duration};
use uuid::Uuid;

async fn create_test_store() -> (PostgresSessionStore, String, String) {
    testing::create_isolated_test_store()
        .await
        .expect("postgres store")
}

async fn cleanup_schema(database_url: &str, schema_name: &str) {
    testing::cleanup_test_schema(database_url, schema_name)
        .await
        .expect("drop schema");
}

async fn with_test_store<F, Fut>(test: F)
where
    F: FnOnce(PostgresSessionStore) -> Fut,
    Fut: Future<Output = ()>,
{
    let (store, database_url, schema_name) = create_test_store().await;
    test(store.clone()).await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

fn qualified(schema_name: &str, table_name: &str) -> String {
    format!("\"{}\".\"{}\"", schema_name, table_name)
}

fn tenant_id(label: &str) -> TenantId {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.bytes().enumerate() {
        bytes[index % 16] = bytes[index % 16].wrapping_mul(31).wrapping_add(byte);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    TenantId::from(Uuid::from_bytes(bytes))
}

fn workspace_key(label: &str) -> StoragePartitionId {
    StoragePartitionId::new(tenant_id(label).to_string())
}

fn test_session_meta(label: &str, model: &str) -> SessionMeta {
    SessionMeta {
        tenant_id: tenant_id(label),
        created_by: Some(SessionActorRef::Identity {
            id: Uuid::from_u128(1),
        }),
        model: ModelId::new(model),
        agent_context: Some(AgentContext::system_default()),
        ..SessionMeta::default()
    }
}

fn test_session_meta_with_id(label: &str, model: &str, id: Uuid) -> SessionMeta {
    SessionMeta {
        id: SessionId(id),
        ..test_session_meta(label, model)
    }
}

async fn set_session_updated_at(
    store: &PostgresSessionStore,
    schema_name: &str,
    session_id: SessionId,
    updated_at: chrono::DateTime<Utc>,
) {
    sqlx::query(&format!(
        "UPDATE {} SET updated_at = $1 WHERE id = $2",
        qualified(schema_name, "sessions")
    ))
    .bind(updated_at)
    .bind(session_id.0)
    .execute(store.pool())
    .await
    .expect("fixture should update session timestamp");
}

fn contact_session_meta(label: &str, model: &str, contact_id: ContactId) -> SessionMeta {
    let tenant_id = tenant_id(label);
    SessionMeta {
        tenant_id,
        contact: Some(ContactRef {
            contact_id,
            tenant_id,
            state: moa_core::types::contact::ContactVerificationState::Verified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: serde_json::json!({}),
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }),
        model: ModelId::new(model),
        agent_context: Some(AgentContext::system_default()),
        ..SessionMeta::default()
    }
}

#[tokio::test]
#[ignore = "requires local Postgres via MOA_DATABASE_URL"]
async fn learning_candidate_summaries_project_contact_scope_and_redact_payload_db() {
    // Pins: the operator candidate inbox projects contact ownership from canonical
    // user scope, tolerates tenant sentinels, redacts payloads, and stays tenant scoped.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant = tenant_id("candidate-summary");
    let other_tenant = tenant_id("candidate-summary-other");
    let contact_id = ContactId::new();
    let now = Utc::now();
    let contact_candidate_id = Uuid::now_v7();
    let tenant_candidate_id = Uuid::now_v7();

    for candidate in [
        LearningCandidate {
            id: contact_candidate_id,
            tenant_id: tenant,
            user_id: Some(UserId::new(contact_id.to_string())),
            candidate_type: LearningCandidateType::Skill,
            status: LearningCandidateStatus::Proposed,
            target_id: Some("skill:contact-summary".to_string()),
            target_label: Some("contact summary".to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload: serde_json::json!({
                "description": "safe preview",
                "api_key": "raw-dashboard-secret"
            }),
            evaluation_payload: None,
            source_experience_ids: Vec::new(),
            confidence: Some(0.9),
            risk_class: LearningRiskClass::Low,
            promotion_requirements: vec!["human_review".to_string()],
            status_reason: None,
            batch_id: None,
            created_at: now,
            updated_at: now + chrono::Duration::seconds(2),
        },
        LearningCandidate {
            id: tenant_candidate_id,
            tenant_id: tenant,
            user_id: Some(UserId::new(format!("tenant:{tenant}"))),
            candidate_type: LearningCandidateType::Prompt,
            status: LearningCandidateStatus::Proposed,
            target_id: None,
            target_label: Some("tenant summary".to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload: serde_json::json!({"description": "tenant safe preview"}),
            evaluation_payload: None,
            source_experience_ids: Vec::new(),
            confidence: None,
            risk_class: LearningRiskClass::Medium,
            promotion_requirements: Vec::new(),
            status_reason: None,
            batch_id: None,
            created_at: now,
            updated_at: now + chrono::Duration::seconds(1),
        },
        LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: other_tenant,
            user_id: None,
            candidate_type: LearningCandidateType::Eval,
            status: LearningCandidateStatus::Proposed,
            target_id: None,
            target_label: Some("other tenant".to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload: serde_json::json!({"description": "must stay hidden"}),
            evaluation_payload: None,
            source_experience_ids: Vec::new(),
            confidence: None,
            risk_class: LearningRiskClass::Low,
            promotion_requirements: Vec::new(),
            status_reason: None,
            batch_id: None,
            created_at: now,
            updated_at: now + chrono::Duration::seconds(3),
        },
    ] {
        store
            .append_learning_candidate(&candidate)
            .await
            .expect("append candidate summary fixture");
    }

    let summaries = store
        .list_learning_candidate_summaries(tenant, Some(LearningCandidateStatus::Proposed), 10)
        .await
        .expect("list tenant candidate summaries");

    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id, contact_candidate_id);
    assert_eq!(summaries[0].contact_id, Some(contact_id));
    assert_eq!(
        summaries[0].payload_preview,
        r#"{"api_key":"[redacted]","description":"safe preview"}"#
    );
    assert!(
        !summaries[0]
            .payload_preview
            .contains("raw-dashboard-secret")
    );
    assert_eq!(summaries[1].id, tenant_candidate_id);
    assert_eq!(summaries[1].contact_id, None);

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn learning_log_round_trips_skill_entry() {
    with_test_store(|store| async move {
        let batch_id = Uuid::now_v7();
        let tenant_id = tenant_id("tenant-learning");
        let entry = LearningEntry {
            id: Uuid::now_v7(),
            tenant_id,
            learning_type: "skill_created".to_string(),
            target_id: "skill:moa-rust".to_string(),
            target_label: Some("moa-rust".to_string()),
            payload: serde_json::json!({ "version": 1, "source": "distillation" }),
            confidence: Some(0.8),
            source_refs: vec![Uuid::now_v7()],
            actor: "system".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            batch_id: Some(batch_id),
            version: 1,
        };
        store
            .append_learning(&entry)
            .await
            .expect("append learning");
        let learnings = store
            .list_learnings(&tenant_id.to_string(), Some("skill_created"), 10)
            .await
            .expect("list learnings");
        assert_eq!(learnings.len(), 1);
        assert_eq!(learnings[0].id, entry.id);
        assert_eq!(learnings[0].target_id, "skill:moa-rust");
        assert_eq!(learnings[0].target_label.as_deref(), Some("moa-rust"));
        assert_eq!(
            learnings[0].payload,
            serde_json::json!({ "version": 1, "source": "distillation" })
        );
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn postgres_shared_session_store_contract() {
    with_test_store(|store| async move {
        test_create_and_get_session(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        test_emit_and_get_events(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        test_event_search(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        test_list_sessions_with_filter(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        test_tenant_cost_since(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        test_session_status_update(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        test_action_policy_rules(&store).await;
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn postgres_create_session_requires_creator_or_contact_and_agent_context() {
    with_test_store(|store| async move {
        let missing_creator = store
            .create_session(SessionMeta {
                created_by: None,
                contact: None,
                ..test_session_meta("pg-session-creator-required", "test-model")
            })
            .await
            .expect_err("missing creator/contact attribution must be rejected");
        assert!(
            matches!(missing_creator, MoaError::ValidationError(ref message) if message.contains("contact or creator")),
            "expected creator/contact validation error, got {missing_creator}"
        );

        let mut missing_agent = test_session_meta("pg-session-agent-required", "test-model");
        missing_agent.agent_context = None;
        let missing_agent = store
            .create_session(missing_agent)
            .await
            .expect_err("missing agent_context must be rejected");
        assert!(
            matches!(missing_agent, MoaError::ValidationError(ref message) if message.contains("agent_context")),
            "expected agent_context validation error, got {missing_agent}"
        );
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn dashboard_session_list_is_tenant_scoped_and_keyset_paginated() {
    // Pins: dashboard session listing is tenant-scoped and uses deterministic
    // `(updated_at, session_id)` keyset pagination for timestamp ties.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant = tenant_id("dashboard-list");
    let other_tenant = tenant_id("dashboard-list-other");
    let high_id = SessionId(Uuid::from_u128(0x00000000000000000000000000000003));
    let mid_id = SessionId(Uuid::from_u128(0x00000000000000000000000000000002));
    let old_id = SessionId(Uuid::from_u128(0x00000000000000000000000000000001));
    let other_id = SessionId(Uuid::from_u128(0x00000000000000000000000000000004));
    let tie_time = chrono::DateTime::parse_from_rfc3339("2026-07-05T12:00:00Z")
        .expect("fixture timestamp should parse")
        .with_timezone(&Utc);
    let old_time = chrono::DateTime::parse_from_rfc3339("2026-07-05T11:59:00Z")
        .expect("fixture timestamp should parse")
        .with_timezone(&Utc);
    let other_time = chrono::DateTime::parse_from_rfc3339("2026-07-05T12:01:00Z")
        .expect("fixture timestamp should parse")
        .with_timezone(&Utc);

    for session_id in [high_id, mid_id, old_id] {
        store
            .create_session(test_session_meta_with_id(
                "dashboard-list",
                "test-model",
                session_id.0,
            ))
            .await
            .expect("create tenant session");
    }
    store
        .create_session(test_session_meta_with_id(
            "dashboard-list-other",
            "test-model",
            other_id.0,
        ))
        .await
        .expect("create other tenant session");

    set_session_updated_at(&store, &schema_name, high_id, tie_time).await;
    set_session_updated_at(&store, &schema_name, mid_id, tie_time).await;
    set_session_updated_at(&store, &schema_name, old_id, old_time).await;
    set_session_updated_at(&store, &schema_name, other_id, other_time).await;

    let first_page = store
        .list_dashboard_sessions(
            tenant,
            DashboardSessionListRequest {
                limit: Some(2),
                ..DashboardSessionListRequest::default()
            },
        )
        .await
        .expect("list first dashboard page");
    let first_ids: Vec<_> = first_page
        .sessions
        .iter()
        .map(|session| session.session_id)
        .collect();
    assert_eq!(first_ids, vec![high_id, mid_id]);
    assert_eq!(
        first_page.next_cursor,
        Some(DashboardSessionListCursor {
            updated_at: tie_time,
            session_id: mid_id,
        })
    );

    let second_page = store
        .list_dashboard_sessions(
            tenant,
            DashboardSessionListRequest {
                limit: Some(2),
                cursor: first_page.next_cursor,
                ..DashboardSessionListRequest::default()
            },
        )
        .await
        .expect("list second dashboard page");
    let second_ids: Vec<_> = second_page
        .sessions
        .iter()
        .map(|session| session.session_id)
        .collect();
    assert_eq!(second_ids, vec![old_id]);
    assert_eq!(second_page.next_cursor, None);
    assert!(
        second_page
            .sessions
            .iter()
            .all(|session| session.tenant_id == tenant),
        "tenant-scoped list must not include another tenant"
    );

    let other_page = store
        .list_dashboard_sessions(
            other_tenant,
            DashboardSessionListRequest {
                limit: Some(10),
                ..DashboardSessionListRequest::default()
            },
        )
        .await
        .expect("list other tenant dashboard page");
    assert_eq!(other_page.sessions.len(), 1);
    assert_eq!(other_page.sessions[0].session_id, other_id);
    assert_eq!(other_page.next_cursor, None);

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn dashboard_session_detail_requires_tenant_scope_and_reports_aggregates() {
    // Pins: dashboard detail reads require the owning tenant and expose aggregate
    // counters through the real session/event write path.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant = tenant_id("dashboard-detail");
    let other_tenant = tenant_id("dashboard-detail-other");
    let session_id = store
        .create_session(test_session_meta("dashboard-detail", "test-model"))
        .await
        .expect("create dashboard detail session");
    store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "ready".to_string(),
                thought_signature: None,
                model: ModelId::new("test-model"),
                model_tier: ModelTier::Main,
                input_tokens_uncached: 4,
                input_tokens_cache_write: 3,
                input_tokens_cache_read: 2,
                output_tokens: 5,
                cost_cents: 7,
                duration_ms: 10,
                llm_ttft_ms: None,
            },
        )
        .await
        .expect("emit aggregate event");

    let detail = store
        .get_dashboard_session_detail(tenant, session_id)
        .await
        .expect("load dashboard session detail")
        .expect("session should be visible in owning tenant");
    assert_eq!(detail.session_id, session_id);
    assert_eq!(detail.tenant_id, tenant);
    assert_eq!(detail.model, ModelId::new("test-model"));
    assert_eq!(detail.event_count, 1);
    assert_eq!(detail.total_input_tokens, 9);
    assert_eq!(detail.total_output_tokens, 5);
    assert_eq!(detail.total_cost_cents, 7);
    assert!((detail.cache_hit_rate - (2.0_f64 / 9.0_f64)).abs() < f64::EPSILON);

    let cross_tenant = store
        .get_dashboard_session_detail(other_tenant, session_id)
        .await
        .expect("cross-tenant dashboard detail should not error");
    assert_eq!(cross_tenant, None);

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn dashboard_event_pages_use_sequence_cursors_and_tenant_scope() {
    // Pins: dashboard event pagination advances by event sequence number and
    // cross-tenant reads return no events for the same session id while summaries
    // redact raw event payload values.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant = tenant_id("dashboard-events");
    let other_tenant = tenant_id("dashboard-events-other");
    let session_id = store
        .create_session(test_session_meta("dashboard-events", "test-model"))
        .await
        .expect("create dashboard events session");
    for index in 0..5 {
        store
            .emit_event(
                session_id,
                Event::UserMessage {
                    text: format!("raw-dashboard-secret-{index}"),
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("emit dashboard event");
    }

    let first_page = store
        .list_dashboard_session_events(
            tenant,
            session_id,
            DashboardEventPageRequest {
                limit: Some(2),
                ..DashboardEventPageRequest::default()
            },
        )
        .await
        .expect("list first event page");
    let first_sequences: Vec<_> = first_page
        .events
        .iter()
        .map(|event| event.sequence_num)
        .collect();
    assert_eq!(first_sequences, vec![0, 1]);
    assert_eq!(
        first_page.next_cursor,
        Some(DashboardEventCursor { sequence_num: 1 })
    );
    assert_eq!(
        first_page
            .events
            .iter()
            .map(|event| event.summary.as_str())
            .collect::<Vec<_>>(),
        vec![
            "user message with 0 attachments",
            "user message with 0 attachments"
        ]
    );
    let first_page_json =
        serde_json::to_string(&first_page).expect("first dashboard event page should serialize");
    assert!(
        !first_page_json.contains("raw-dashboard-secret"),
        "redacted event summaries must not expose raw payload values: {first_page_json}"
    );

    let second_page = store
        .list_dashboard_session_events(
            tenant,
            session_id,
            DashboardEventPageRequest {
                limit: Some(2),
                cursor: first_page.next_cursor,
                ..DashboardEventPageRequest::default()
            },
        )
        .await
        .expect("list second event page");
    let second_sequences: Vec<_> = second_page
        .events
        .iter()
        .map(|event| event.sequence_num)
        .collect();
    assert_eq!(second_sequences, vec![2, 3]);
    assert_eq!(
        second_page.next_cursor,
        Some(DashboardEventCursor { sequence_num: 3 })
    );

    let final_page = store
        .list_dashboard_session_events(
            tenant,
            session_id,
            DashboardEventPageRequest {
                limit: Some(2),
                cursor: second_page.next_cursor,
                ..DashboardEventPageRequest::default()
            },
        )
        .await
        .expect("list final event page");
    let final_sequences: Vec<_> = final_page
        .events
        .iter()
        .map(|event| event.sequence_num)
        .collect();
    assert_eq!(final_sequences, vec![4]);
    assert_eq!(final_page.next_cursor, None);

    let cross_tenant = store
        .list_dashboard_session_events(
            other_tenant,
            session_id,
            DashboardEventPageRequest {
                limit: Some(10),
                ..DashboardEventPageRequest::default()
            },
        )
        .await
        .expect("cross-tenant dashboard events should not error");
    assert_eq!(cross_tenant.events, Vec::new());
    assert_eq!(cross_tenant.next_cursor, None);

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_direct_session_insert_requires_agent_context_sidecar() {
    let (store, database_url, schema_name) = create_test_store().await;
    let sessions = qualified(&schema_name, "sessions");
    let tenant_id = tenant_id("pg-session-db-agent-required");
    let mut tx = store
        .pool()
        .begin()
        .await
        .expect("begin direct insert transaction");
    sqlx::query(&format!(
        "INSERT INTO {sessions} \
         (id, storage_partition_id, user_id, status, channel, model, created_at, updated_at) \
         VALUES ($1, $2, $3, 'created', 'chat', 'test-model', NOW(), NOW())"
    ))
    .bind(Uuid::now_v7())
    .bind(tenant_id.to_string())
    .bind("user")
    .execute(&mut *tx)
    .await
    .expect("direct session insert should be accepted until deferred commit check");

    let error = tx
        .commit()
        .await
        .expect_err("commit without session_agent_context must fail");
    let database_error = error
        .as_database_error()
        .expect("expected database constraint error");
    assert_eq!(database_error.code().as_deref(), Some("23514"));
    assert!(
        database_error
            .message()
            .contains("missing required agent context"),
        "unexpected constraint message: {}",
        database_error.message()
    );

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_event_payloads_round_trip_as_jsonb() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-jsonb", "test-model"))
        .await
        .expect("create session");

    let tool_uuid = Uuid::now_v7();
    let tool_id = moa_core::types::identifiers::ToolCallId(tool_uuid);
    let output = ToolOutput::json(
        "structured",
        serde_json::json!({
            "nested": { "value": 42, "ok": true },
            "items": ["a", "b", "c"]
        }),
        Duration::from_millis(25),
    );
    store
        .emit_event(
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some("toolu_jsonb".to_string()),
                output: output.clone(),
                original_output_tokens: None,
                success: true,
                duration_ms: 25,
            },
        )
        .await
        .expect("emit tool result");

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let payload: serde_json::Value = sqlx::query_scalar(&format!(
        "SELECT payload FROM {} LIMIT 1",
        qualified(&schema_name, "events")
    ))
    .fetch_one(&pool)
    .await
    .expect("fetch payload");
    let jsonb_type: String = sqlx::query_scalar(&format!(
        "SELECT pg_typeof(payload)::text FROM {} LIMIT 1",
        qualified(&schema_name, "events")
    ))
    .fetch_one(&pool)
    .await
    .expect("fetch payload type");

    assert_eq!(jsonb_type, "jsonb");
    assert_eq!(payload["type"], "ToolResult");
    assert_eq!(payload["data"]["tool_id"], tool_id.to_string());
    assert_eq!(
        payload["data"]["output"]["structured"]["nested"]["value"],
        serde_json::json!(42)
    );

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_tool_event_exists_matches_session_workspace_type_and_tool_id() {
    let (store, database_url, schema_name) = create_test_store().await;
    let storage_partition_id = workspace_key("pg-tool-event-exists");
    let other_storage_partition_id = workspace_key("pg-tool-event-exists-other");
    let session_id = store
        .create_session(test_session_meta("pg-tool-event-exists", "test-model"))
        .await
        .expect("create session");
    let other_session_id = store
        .create_session(test_session_meta("pg-tool-event-exists", "test-model"))
        .await
        .expect("create other session");
    let tool_id = ToolCallId(Uuid::now_v7());
    let other_session_tool_id = ToolCallId(Uuid::now_v7());
    let output = ToolOutput::text("ok", Duration::from_millis(5));

    store
        .emit_event(
            session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: Some("toolu_call".to_string()),
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: serde_json::json!({"cmd": "pwd"}),
                hand_id: None,
            },
        )
        .await
        .expect("emit tool call");
    store
        .emit_event(
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some("toolu_call".to_string()),
                output,
                original_output_tokens: None,
                success: true,
                duration_ms: 5,
            },
        )
        .await
        .expect("emit tool result");
    store
        .emit_event(
            other_session_id,
            Event::ToolCall {
                tool_id: other_session_tool_id,
                provider_tool_use_id: Some("toolu_other".to_string()),
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: serde_json::json!({"cmd": "date"}),
                hand_id: None,
            },
        )
        .await
        .expect("emit other session tool call");

    assert!(
        store
            .tool_event_exists(
                &storage_partition_id,
                session_id,
                EventType::ToolCall,
                tool_id
            )
            .await
            .expect("tool call existence query")
    );
    assert!(
        store
            .tool_event_exists(
                &storage_partition_id,
                session_id,
                EventType::ToolResult,
                tool_id
            )
            .await
            .expect("tool result existence query")
    );
    assert!(
        !store
            .tool_event_exists(
                &storage_partition_id,
                session_id,
                EventType::ToolError,
                tool_id
            )
            .await
            .expect("wrong event type query")
    );
    assert!(
        !store
            .tool_event_exists(
                &storage_partition_id,
                session_id,
                EventType::ToolCall,
                other_session_tool_id,
            )
            .await
            .expect("other session tool query")
    );
    assert!(
        !store
            .tool_event_exists(
                &other_storage_partition_id,
                session_id,
                EventType::ToolCall,
                tool_id
            )
            .await
            .expect("wrong workspace query")
    );

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_task_segments_track_boundaries_and_usage() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-segments", "test-model"))
        .await
        .expect("create session");
    let first_id = deterministic_segment_id(session_id, 0);
    let second_id = deterministic_segment_id(session_id, 1);
    let now = Utc::now();

    store
        .create_segment(&TaskSegment {
            id: first_id,
            session_id,
            tenant_id: "pg-segments".to_string(),
            segment_index: 0,
            task_summary: Some("Fix tests".to_string()),
            started_at: now,
            ended_at: None,
            turn_count: 0,
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 0,
            previous_segment_id: None,
            outcome: None,
            assessment: None,
            outcome_confidence: None,
        })
        .await
        .expect("create first segment");
    store
        .record_active_segment_tool_use(session_id, "bash")
        .await
        .expect("record tool");
    store
        .record_active_segment_skill_activation(session_id, "moa-rust")
        .await
        .expect("record skill");
    // `moa-rust` was injected AND engaged; `moa-idle` is injected-only below, so used
    // must be the strict subset the model actually engaged.
    store
        .record_active_segment_skill_activation(session_id, "moa-idle")
        .await
        .expect("record injected-only skill");
    store
        .record_active_segment_skill_use(session_id, "moa-rust")
        .await
        .expect("record skill use");
    store
        .record_active_segment_turn_usage(session_id, 250)
        .await
        .expect("record usage");

    let active = store
        .get_active_segment(session_id)
        .await
        .expect("load active")
        .expect("active segment exists");
    assert_eq!(active.tools_used, vec!["bash".to_string()]);
    assert_eq!(
        active.skills_activated,
        vec!["moa-rust".to_string(), "moa-idle".to_string()]
    );
    assert_eq!(active.skills_used, vec!["moa-rust".to_string()]);
    assert_eq!(active.turn_count, 1);
    assert_eq!(active.token_cost, 250);

    store
        .complete_segment(
            first_id,
            SegmentCompletion {
                ended_at: Utc::now(),
                turn_count: active.turn_count,
                tools_used: active.tools_used,
                skills_activated: active.skills_activated,
                skills_used: active.skills_used,
                token_cost: active.token_cost,
            },
        )
        .await
        .expect("complete first segment");
    store
        .create_segment(&TaskSegment {
            id: second_id,
            session_id,
            tenant_id: "pg-segments".to_string(),
            segment_index: 1,
            task_summary: Some("Update README".to_string()),
            started_at: Utc::now(),
            ended_at: None,
            turn_count: 0,
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 0,
            previous_segment_id: Some(first_id),
            outcome: None,
            assessment: None,
            outcome_confidence: None,
        })
        .await
        .expect("create second segment");

    let segments = store
        .list_segments(session_id)
        .await
        .expect("list segments");
    assert_eq!(segments.len(), 2);
    assert!(segments[0].ended_at.is_some());
    // The completed segment persisted skills_used distinctly from skills_activated.
    assert_eq!(
        segments[0].skills_activated,
        vec!["moa-rust".to_string(), "moa-idle".to_string()]
    );
    assert_eq!(segments[0].skills_used, vec!["moa-rust".to_string()]);
    assert_eq!(segments[1].previous_segment_id, Some(first_id));
    assert_eq!(segments[1].outcome, None);

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_session_owned_writes_fail_when_session_is_missing() {
    let (store, database_url, schema_name) = create_test_store().await;
    let missing_session = SessionId::new();
    let now = Utc::now();

    let snapshot_error = store
        .put_snapshot(
            missing_session,
            ContextSnapshot {
                format_version: moa_core::types::snapshot::CONTEXT_SNAPSHOT_FORMAT_VERSION,
                session_id: missing_session,
                last_sequence_num: 0,
                created_at: now,
                messages: Vec::new(),
                file_read_dedup_state: FileReadDedupState::default(),
                token_count: 0,
                stage_inputs_hash: 0,
            },
        )
        .await
        .expect_err("snapshot write must reject a missing session");
    assert!(
        matches!(snapshot_error, MoaError::SessionNotFound(id) if id == missing_session),
        "unexpected snapshot error: {snapshot_error:?}"
    );

    let segment_error = store
        .create_segment(&TaskSegment {
            id: deterministic_segment_id(missing_session, 0),
            session_id: missing_session,
            tenant_id: "missing-session".to_string(),
            segment_index: 0,
            task_summary: Some("missing parent".to_string()),
            started_at: now,
            ended_at: None,
            turn_count: 0,
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 0,
            previous_segment_id: None,
            outcome: None,
            assessment: None,
            outcome_confidence: None,
        })
        .await
        .expect_err("segment write must reject a missing session");
    assert!(
        matches!(segment_error, MoaError::SessionNotFound(id) if id == missing_session),
        "unexpected segment error: {segment_error:?}"
    );

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_context_snapshot_upserts_overwrites_and_deletes() {
    // Pins the compaction snapshot lifecycle: a fresh session has no snapshot;
    // put_snapshot inserts and round-trips through JSONB; a second put with a new
    // last_sequence_num overwrites the single row in place (ON CONFLICT DO UPDATE,
    // not a duplicate insert); delete_snapshot removes it; get then returns None.
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-snapshot", "test-model"))
        .await
        .expect("create session");
    let now = Utc::now();

    assert!(
        store
            .get_snapshot(session_id)
            .await
            .expect("get missing snapshot")
            .is_none(),
        "a fresh session must have no snapshot"
    );

    let snapshot = ContextSnapshot {
        format_version: moa_core::types::snapshot::CONTEXT_SNAPSHOT_FORMAT_VERSION,
        session_id,
        last_sequence_num: 3,
        created_at: now,
        messages: Vec::new(),
        file_read_dedup_state: FileReadDedupState::default(),
        token_count: 11,
        stage_inputs_hash: 7,
    };
    store
        .put_snapshot(session_id, snapshot.clone())
        .await
        .expect("put snapshot");

    let loaded = store
        .get_snapshot(session_id)
        .await
        .expect("get snapshot")
        .expect("snapshot present after put");
    assert_eq!(loaded, snapshot, "snapshot must round-trip through JSONB");

    let updated = ContextSnapshot {
        last_sequence_num: 9,
        token_count: 42,
        stage_inputs_hash: 21,
        ..snapshot.clone()
    };
    store
        .put_snapshot(session_id, updated.clone())
        .await
        .expect("overwrite snapshot");

    let reloaded = store
        .get_snapshot(session_id)
        .await
        .expect("get overwritten snapshot")
        .expect("snapshot present after overwrite");
    assert_eq!(
        reloaded, updated,
        "second put must overwrite the snapshot in place"
    );

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let snapshot_rows: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {} WHERE session_id = $1",
        qualified(&schema_name, "context_snapshots")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("count snapshot rows");
    assert_eq!(
        snapshot_rows, 1,
        "overwrite must keep exactly one snapshot row, not duplicate it"
    );
    pool.close().await;

    store
        .delete_snapshot(session_id)
        .await
        .expect("delete snapshot");
    assert!(
        store
            .get_snapshot(session_id)
            .await
            .expect("get after delete")
            .is_none(),
        "snapshot must be gone after delete"
    );

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_snapshot_checkpoint_bounds_event_replay() {
    // Pins checkpoint-bounded replay: after a snapshot is taken at sequence K, the
    // resume path reads only events strictly after K (from_seq = K + 1), so
    // pre-checkpoint events are excluded from replay and only post-checkpoint events
    // remain to be reapplied on top of the compacted snapshot.
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-checkpoint", "test-model"))
        .await
        .expect("create session");

    let mut emitted_seqs = Vec::new();
    for index in 0..6 {
        let seq = store
            .emit_event(
                session_id,
                Event::UserMessage {
                    text: format!("message-{index}"),
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("emit user message");
        emitted_seqs.push(seq);
    }

    // Checkpoint mid-stream at the third emitted event.
    let checkpoint_seq = emitted_seqs[2];
    store
        .put_snapshot(
            session_id,
            ContextSnapshot {
                format_version: moa_core::types::snapshot::CONTEXT_SNAPSHOT_FORMAT_VERSION,
                session_id,
                last_sequence_num: checkpoint_seq,
                created_at: Utc::now(),
                messages: Vec::new(),
                file_read_dedup_state: FileReadDedupState::default(),
                token_count: 0,
                stage_inputs_hash: 0,
            },
        )
        .await
        .expect("put checkpoint snapshot");

    let loaded = store
        .get_snapshot(session_id)
        .await
        .expect("get checkpoint snapshot")
        .expect("checkpoint snapshot present");
    assert_eq!(
        loaded.last_sequence_num, checkpoint_seq,
        "snapshot must bound replay at the checkpoint sequence"
    );

    let resume = store
        .get_events(
            session_id,
            moa_core::types::events_stream::EventRange {
                from_seq: Some(loaded.last_sequence_num + 1),
                ..moa_core::types::events_stream::EventRange::all()
            },
        )
        .await
        .expect("read resume events");

    let resumed_seqs: Vec<u64> = resume.iter().map(|record| record.sequence_num).collect();
    let expected_seqs: Vec<u64> = emitted_seqs
        .iter()
        .copied()
        .filter(|seq| *seq > checkpoint_seq)
        .collect();
    assert_eq!(
        resumed_seqs, expected_seqs,
        "resume must read exactly the post-checkpoint events"
    );
    assert!(
        resume
            .iter()
            .all(|record| record.sequence_num > checkpoint_seq),
        "no event at or before the checkpoint may be replayed: {resumed_seqs:?}"
    );
    assert!(
        emitted_seqs[..=2]
            .iter()
            .all(|seq| !resumed_seqs.contains(seq)),
        "pre-checkpoint events must be excluded from replay"
    );

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_task_segment_assessments_and_views_refresh() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-outcome", "test-model"))
        .await
        .expect("create session");
    let now = Utc::now();

    for index in 0..20 {
        let segment_id = deterministic_segment_id(session_id, index);
        let previous_segment_id =
            (index > 0).then(|| deterministic_segment_id(session_id, index - 1));
        store
            .create_segment(&TaskSegment {
                id: segment_id,
                session_id,
                tenant_id: "pg-outcome".to_string(),
                segment_index: index,
                task_summary: Some(format!("Task {index}")),
                started_at: now + chrono::Duration::seconds(i64::from(index)),
                ended_at: None,
                turn_count: 0,
                tools_used: vec!["bash".to_string()],
                skills_activated: vec!["moa-rust".to_string()],
                skills_used: vec!["moa-rust".to_string()],
                token_cost: 0,
                previous_segment_id,
                outcome: None,
                assessment: None,
                outcome_confidence: None,
            })
            .await
            .expect("create segment");
        store
            .complete_segment(
                segment_id,
                SegmentCompletion {
                    ended_at: now + chrono::Duration::seconds(i64::from(index + 10)),
                    turn_count: 2,
                    tools_used: vec!["bash".to_string()],
                    skills_activated: vec!["moa-rust".to_string()],
                    skills_used: vec!["moa-rust".to_string()],
                    token_cost: 500,
                },
            )
            .await
            .expect("complete segment");
        store
            .update_segment_assessment(
                segment_id,
                &SegmentAssessment {
                    outcome: SegmentOutcome::Resolved,
                    confidence: 0.92,
                    phase: AssessmentPhase::Immediate,
                    evidence: vec![
                        SegmentEvidence {
                            kind: SegmentEvidenceKind::ToolOutcome,
                            polarity: SegmentEvidencePolarity::SupportsResolved,
                            strength: 0.8,
                            summary: "tool outcome signal".to_string(),
                        },
                        SegmentEvidence {
                            kind: SegmentEvidenceKind::Verification,
                            polarity: SegmentEvidencePolarity::SupportsResolved,
                            strength: 0.95,
                            summary: "verification command signal".to_string(),
                        },
                    ],
                    assessed_at: Utc::now(),
                    policy_version: "segment-assessment-test".to_string(),
                },
            )
            .await
            .expect("update segment assessment");
    }

    let first = store
        .list_segments(session_id)
        .await
        .expect("list segments")
        .into_iter()
        .next()
        .expect("first segment exists");
    assert_eq!(first.outcome.as_deref(), Some("resolved"));
    let assessment = first.assessment.as_ref().expect("assessment persisted");
    assert_eq!(assessment.outcome, SegmentOutcome::Resolved);
    assert_eq!(assessment.phase, AssessmentPhase::Immediate);
    assert_eq!(assessment.evidence.len(), 2);
    assert_eq!(first.outcome_confidence, Some(0.92));

    store
        .refresh_segment_materialized_views()
        .await
        .expect("refresh outcome views");
    let rates = store
        .list_skill_resolution_rates("pg-outcome")
        .await
        .expect("list outcome rates");
    assert_eq!(rates.len(), 1);
    assert_eq!(rates[0].skill_name, "moa-rust");
    assert_eq!(rates[0].uses, 20);
    assert!((rates[0].resolution_rate - 1.0_f64).abs() < f64::EPSILON);

    let baseline = store
        .get_segment_baseline("pg-outcome")
        .await
        .expect("load baseline")
        .expect("baseline exists");
    assert_eq!(baseline.sample_count, 20);
    assert!((baseline.avg_turns - 2.0_f64).abs() < f64::EPSILON);
    assert!((baseline.avg_cost - 500.0_f64).abs() < f64::EPSILON);

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn experience_records_and_candidates_round_trip() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-experience", "test-model"))
        .await
        .expect("create session");
    let segment_id = deterministic_segment_id(session_id, 0);
    let now = Utc::now();
    let tenant_id = tenant_id("pg-experience");
    let assessment = SegmentAssessment {
        outcome: SegmentOutcome::Resolved,
        confidence: 0.9,
        phase: AssessmentPhase::Immediate,
        evidence: vec![SegmentEvidence {
            kind: SegmentEvidenceKind::Verification,
            polarity: SegmentEvidencePolarity::SupportsResolved,
            strength: 0.95,
            summary: "cargo test passed".to_string(),
        }],
        assessed_at: now,
        policy_version: "assessment-test".to_string(),
    };
    store
        .create_segment(&TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: tenant_id.to_string(),
            segment_index: 0,
            task_summary: Some("Fix Rust auth failure".to_string()),
            started_at: now,
            ended_at: Some(now),
            turn_count: 2,
            tools_used: vec!["bash".to_string()],
            skills_activated: vec!["moa-rust".to_string()],
            skills_used: vec!["moa-rust".to_string()],
            token_cost: 500,
            previous_segment_id: None,
            outcome: Some("resolved".to_string()),
            assessment: Some(assessment.clone()),
            outcome_confidence: Some(assessment.confidence),
        })
        .await
        .expect("create assessed segment");

    let fingerprint = TaskFingerprint {
        hash: "task-hash".to_string(),
        normalized_summary: "auth failure fix rust".to_string(),
        policy_version: "experience_v1".to_string(),
    };
    let facets = TaskFacetSet {
        domain: Some("auth".to_string()),
        action: Some("debug".to_string()),
        artifact_kind: Some("code".to_string()),
        language_or_framework: Some("rust".to_string()),
        verification_style: Some("command".to_string()),
        risk_class: Some("high".to_string()),
        tool_pattern: vec!["bash".to_string()],
        skill_pattern: vec!["moa-rust".to_string()],
    };
    let experience = ExperienceRecord {
        id: Uuid::now_v7(),
        segment_id,
        session_id,
        tenant_id,
        user_id: UserId::new("user"),
        task_summary: Some("Fix Rust auth failure".to_string()),
        task_fingerprint: fingerprint.clone(),
        task_facets: facets.clone(),
        actions: vec!["debug".to_string()],
        resources: Vec::new(),
        outcome: SegmentOutcome::Resolved,
        confidence: 0.9,
        evidence: assessment.evidence.clone(),
        tools_used: vec!["bash".to_string()],
        skills_activated: vec!["moa-rust".to_string()],
        skills_used: vec!["moa-rust".to_string()],
        turn_count: 2,
        token_cost: 500,
        duration_ms: Some(1_000),
        assessment_policy_version: assessment.policy_version.clone(),
        extraction_policy_version: "experience_v1".to_string(),
        created_at: now,
    };
    store
        .append_experience_record(&experience)
        .await
        .expect("append experience");
    let attribution = ExperienceAttribution {
        id: Uuid::now_v7(),
        experience_id: experience.id,
        tenant_id,
        user_id: Some(UserId::new("user")),
        subject_type: AttributionSubjectType::Skill,
        subject_id: "moa-rust".to_string(),
        effect: AttributionEffect::Helpful,
        kind: AttributionKind::Standard,
        confidence: 0.9,
        evidence: vec!["skill was active during a resolved segment".to_string()],
        created_at: now,
    };
    // A skill injected into the same segment but never engaged: a separate subject row
    // that must be counted only as an unused injection, never in uses/success_rate.
    let unused_attribution = ExperienceAttribution {
        id: Uuid::now_v7(),
        experience_id: experience.id,
        tenant_id,
        user_id: Some(UserId::new("user")),
        subject_type: AttributionSubjectType::Skill,
        subject_id: "moa-unused".to_string(),
        effect: AttributionEffect::Neutral,
        kind: AttributionKind::UnusedInjection,
        confidence: 0.9,
        evidence: vec!["skill was injected but never engaged".to_string()],
        created_at: now,
    };
    store
        .append_experience_attributions(&[attribution, unused_attribution])
        .await
        .expect("append attribution");
    let candidate = LearningCandidate {
        id: Uuid::now_v7(),
        tenant_id,
        user_id: None,
        candidate_type: LearningCandidateType::Skill,
        status: LearningCandidateStatus::Proposed,
        target_id: Some("skills/moa-rust/SKILL.md".to_string()),
        target_label: Some("moa-rust".to_string()),
        task_fingerprint: Some(fingerprint.clone()),
        task_facets: Some(facets),
        payload: serde_json::json!({"skill_markdown": "# moa-rust"}),
        evaluation_payload: None,
        source_experience_ids: vec![experience.id],
        confidence: Some(0.9),
        risk_class: LearningRiskClass::Medium,
        promotion_requirements: vec!["regression_comparison".to_string()],
        status_reason: None,
        batch_id: None,
        created_at: now,
        updated_at: now,
    };
    store
        .append_learning_candidate(&candidate)
        .await
        .expect("append candidate");
    store
        .update_learning_candidate_status(&LearningCandidateStatusUpdate {
            candidate_id: candidate.id,
            status: LearningCandidateStatus::Evaluating,
            status_reason: Some("running regression".to_string()),
            evaluation_payload: Some(serde_json::json!({"suite": "generated"})),
            updated_at: Utc::now(),
        })
        .await
        .expect("candidate status update");
    let mut duplicate_candidate = candidate.clone();
    duplicate_candidate.payload = serde_json::json!({"skill_markdown": "# moa-rust\nupdated"});
    duplicate_candidate.status = LearningCandidateStatus::Proposed;
    duplicate_candidate.updated_at = Utc::now();
    store
        .append_learning_candidate(&duplicate_candidate)
        .await
        .expect("idempotent candidate append should not reset status");

    let experiences = store
        .list_experience_records(session_id)
        .await
        .expect("list experiences");
    assert_eq!(experiences.len(), 1);
    assert_eq!(experiences[0].id, experience.id);
    assert_eq!(experiences[0].task_fingerprint.hash, "task-hash");
    assert_eq!(experiences[0].task_facets.domain.as_deref(), Some("auth"));
    let loaded_experience = store
        .get_experience_record(session_id, experience.id)
        .await
        .expect("load experience by id")
        .expect("experience exists");
    assert_eq!(loaded_experience.id, experience.id);
    assert_eq!(loaded_experience.session_id, session_id);
    let attributions = store
        .list_experience_attributions(experience.id)
        .await
        .expect("list attributions");
    assert_eq!(attributions.len(), 2);
    assert!(attributions.iter().any(
        |row| row.subject_id == "moa-rust" && row.subject_type == AttributionSubjectType::Skill
    ));
    let candidates = store
        .list_learning_candidates(
            &tenant_id.to_string(),
            Some(LearningCandidateStatus::Evaluating),
            10,
        )
        .await
        .expect("list candidates");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, candidate.id);
    assert_eq!(
        candidates[0].evaluation_payload,
        Some(serde_json::json!({"suite": "generated"}))
    );

    store
        .refresh_segment_materialized_views()
        .await
        .expect("refresh experience views");
    let rates = store
        .list_task_strategy_success_rates(&tenant_id.to_string(), "task-hash")
        .await
        .expect("list task strategy rates");
    // Both the engaged skill and the unused-injection skill surface as separate rows.
    assert_eq!(rates.len(), 2);
    let engaged = rates
        .iter()
        .find(|rate| rate.subject_id == "moa-rust")
        .expect("engaged skill rate");
    assert_eq!(engaged.subject_type, AttributionSubjectType::Skill);
    assert_eq!(engaged.uses, 1);
    assert!((engaged.success_rate - 1.0_f64).abs() < f64::EPSILON);
    assert!((engaged.avg_confidence - 0.9_f64).abs() < f64::EPSILON);
    // Helpful attribution -> effect_score 1.0; no unused injections for this subject.
    assert!((engaged.effect_score - 1.0_f64).abs() < f64::EPSILON);
    assert_eq!(engaged.unused_injections, 0);

    // The injected-but-unused skill contributes only an unused_injection count: its
    // uses stay zero (excluded from success_rate) and effect_score falls back to the
    // 0.5 neutral prior since it has no engaged rows.
    let unused = rates
        .iter()
        .find(|rate| rate.subject_id == "moa-unused")
        .expect("unused-injection skill rate");
    assert_eq!(unused.uses, 0);
    assert_eq!(unused.unused_injections, 1);
    assert!((unused.success_rate - 0.0_f64).abs() < f64::EPSILON);
    assert!((unused.effect_score - 0.5_f64).abs() < f64::EPSILON);

    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_store_learning_candidate_review_lookup() {
    with_test_store(|store| async move {
        let review_tenant_id = tenant_id("pg-review");
        let other_tenant_id = tenant_id("pg-review-other");
        let source_experience_id = Uuid::now_v7();
        let artifact_uid = Uuid::now_v7();
        let draft_revision_uid = Uuid::now_v7();
        let payload = serde_json::json!({
            "kind": "skill_draft_proposal",
            "operation": "skill_created",
            "artifact_uid": artifact_uid.to_string(),
            "draft_artifact_revision_uid": draft_revision_uid.to_string(),
            "package": {
                "files": [
                    {
                        "path": "SKILL.md",
                        "source": "# Reviewable Skill\n\nUse this when the reviewed task recurs."
                    }
                ]
            }
        });
        let now = Utc::now();
        let candidate = LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: review_tenant_id,
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            status: LearningCandidateStatus::Proposed,
            target_id: Some("skills/reviewable-skill".to_string()),
            target_label: Some("reviewable-skill".to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload: payload.clone(),
            evaluation_payload: None,
            source_experience_ids: vec![source_experience_id],
            confidence: Some(0.82),
            risk_class: LearningRiskClass::Low,
            promotion_requirements: vec!["human_review".to_string()],
            status_reason: None,
            batch_id: None,
            created_at: now,
            updated_at: now,
        };

        store
            .append_learning_candidate(&candidate)
            .await
            .expect("append review candidate");

        // Pins: review services can load full proposed candidates by id before accepting or rejecting them.
        let loaded = store
            .get_learning_candidate(&candidate.tenant_id, candidate.id)
            .await
            .expect("load candidate by id")
            .expect("candidate exists in owning tenant");
        assert_eq!(loaded.id, candidate.id);
        assert_eq!(loaded.tenant_id, review_tenant_id);
        assert_eq!(loaded.candidate_type, LearningCandidateType::Skill);
        assert_eq!(loaded.status, LearningCandidateStatus::Proposed);
        assert_eq!(loaded.target_label.as_deref(), Some("reviewable-skill"));
        assert_eq!(loaded.payload, payload);
        assert_eq!(loaded.source_experience_ids, vec![source_experience_id]);
        assert_eq!(loaded.risk_class, LearningRiskClass::Low);

        let stale_update = LearningCandidateStatusUpdate {
            candidate_id: candidate.id,
            status: LearningCandidateStatus::Rejected,
            status_reason: Some("stale reviewer".to_string()),
            evaluation_payload: Some(serde_json::json!({"reviewer_subject": "user:stale"})),
            updated_at: Utc::now(),
        };
        let stale_changed = store
            .update_learning_candidate_status_from(
                &stale_update,
                LearningCandidateStatus::Evaluating,
            )
            .await
            .expect("stale compare-and-set status update");
        assert!(
            !stale_changed,
            "status update with the wrong expected state must not change the candidate"
        );
        let still_proposed = store
            .get_learning_candidate(&candidate.tenant_id, candidate.id)
            .await
            .expect("reload proposed candidate")
            .expect("candidate remains visible");
        assert_eq!(still_proposed.status, LearningCandidateStatus::Proposed);
        assert_eq!(still_proposed.evaluation_payload, None);

        let cross_tenant = store
            .get_learning_candidate(&other_tenant_id, candidate.id)
            .await
            .expect("cross-tenant lookup should not fail");
        assert_eq!(cross_tenant, None);

        let evaluation_payload = serde_json::json!({
            "reviewer_subject": "user:reviewer",
            "decision": "reject"
        });
        store
            .update_learning_candidate_status(&LearningCandidateStatusUpdate {
                candidate_id: candidate.id,
                status: LearningCandidateStatus::Rejected,
                status_reason: Some("needs clearer evidence".to_string()),
                evaluation_payload: Some(evaluation_payload.clone()),
                updated_at: Utc::now(),
            })
            .await
            .expect("reject candidate");

        let rejected = store
            .get_learning_candidate(&candidate.tenant_id, candidate.id)
            .await
            .expect("reload rejected candidate")
            .expect("candidate remains visible in owning workspace");
        assert_eq!(rejected.status, LearningCandidateStatus::Rejected);
        assert_eq!(
            rejected.status_reason.as_deref(),
            Some("needs clearer evidence")
        );
        assert_eq!(rejected.evaluation_payload, Some(evaluation_payload));
        assert_eq!(rejected.payload, payload);
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-concurrency", "test-model"))
        .await
        .expect("create session");

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let id_type: String = sqlx::query_scalar(&format!(
        "SELECT pg_typeof(id)::text FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch id type");
    assert_eq!(id_type, "uuid");

    let mut tasks = Vec::new();
    for index in 0..10 {
        let store = store.clone();

        tasks.push(tokio::spawn(async move {
            store
                .emit_event(
                    session_id,
                    Event::UserMessage {
                        text: format!("parallel {index}"),
                        attachments: vec![],
                    },
                )
                .await
        }));
    }

    let mut sequences = Vec::new();
    for task in tasks {
        sequences.push(task.await.expect("join task").expect("emit event"));
    }
    sequences.sort_unstable();
    assert_eq!(sequences, (0..10).collect::<Vec<_>>());

    let event_count: i64 = sqlx::query_scalar(&format!(
        "SELECT event_count FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch event_count");
    assert_eq!(event_count, 10);

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_connection_retry_surfaces_final_failure() {
    let mut config = moa_config::MoaConfig::default();
    config.database.url = "postgres://127.0.0.1:1/moa_test".to_string();
    config.database.connect_timeout_seconds = 1;
    let error = match PostgresSessionStore::from_config(&config).await {
        Ok(_) => panic!("invalid endpoint should fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("after 3 attempts"));
}

#[tokio::test]
#[ignore]
async fn postgres_trigger_populates_generated_session_rollups() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("analytics-ws", "test-model"))
        .await
        .expect("create session");

    for (uncached, cache_write, cache_read, output, cost) in [
        (10usize, 5usize, 15usize, 4usize, 20u32),
        (20usize, 0usize, 10usize, 6usize, 40u32),
        (0usize, 5usize, 5usize, 3usize, 10u32),
    ] {
        store
            .emit_event(
                session_id,
                Event::BrainResponse {
                    text: "turn".to_string(),
                    thought_signature: None,
                    model: "test-model".into(),
                    model_tier: moa_core::types::provider::ModelTier::Main,
                    input_tokens_uncached: uncached,
                    input_tokens_cache_write: cache_write,
                    input_tokens_cache_read: cache_read,
                    output_tokens: output,
                    cost_cents: cost,
                    duration_ms: 100,
                    llm_ttft_ms: None,
                },
            )
            .await
            .expect("emit brain response");
    }

    let summary = store
        .get_session_summary(session_id)
        .await
        .expect("load session summary");
    assert_eq!(summary.turn_count, 3);
    assert_eq!(summary.total_input_tokens, 70);
    assert_eq!(summary.total_output_tokens, 13);
    assert_eq!(summary.total_cost_cents, 70);
    assert!(approx_eq(summary.cache_hit_rate, 30.0 / 70.0, 1e-9));

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let (turn_count, cache_hit_rate): (i64, f64) = sqlx::query_as(&format!(
        "SELECT turn_count, cache_hit_rate FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch generated session columns");
    assert_eq!(turn_count, 3);
    assert!(approx_eq(cache_hit_rate, 30.0 / 70.0, 1e-9));

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_session_summary_tracks_model_tier_costs() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("tiered-costs-ws", "claude-sonnet-4-6"))
        .await
        .expect("create session");

    let tool_id = ToolCallId::new();
    store
        .emit_event(
            session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: serde_json::json!({ "cmd": "echo hi" }),
                hand_id: None,
            },
        )
        .await
        .expect("emit tool call");
    store
        .emit_event(
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text("hi", Duration::from_millis(10)),
                original_output_tokens: None,
                success: true,
                duration_ms: 10,
            },
        )
        .await
        .expect("emit tool result");
    store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "main turn".to_string(),
                thought_signature: None,
                model: "claude-sonnet-4-6".into(),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 12,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 6,
                cost_cents: 20,
                duration_ms: 30,
                llm_ttft_ms: None,
            },
        )
        .await
        .expect("emit brain response");
    store
        .emit_event(
            session_id,
            Event::Checkpoint {
                summary: "summarized prior turns".to_string(),
                events_summarized: 2,
                token_count: 8,
                model: "claude-haiku-4-5".into(),
                model_tier: moa_core::types::provider::ModelTier::Auxiliary,
                input_tokens: 9,
                output_tokens: 4,
                cost_cents: 6,
            },
        )
        .await
        .expect("emit checkpoint");

    let summary = store
        .get_session_summary(session_id)
        .await
        .expect("load session summary");
    assert_eq!(summary.total_cost_cents, 26);
    assert_eq!(summary.main_cost_cents, 20);
    assert_eq!(summary.auxiliary_cost_cents, 6);

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let (main_cost_cents, auxiliary_cost_cents): (i64, i64) = sqlx::query_as(&format!(
        "SELECT main_cost_cents, auxiliary_cost_cents FROM {} WHERE id = $1",
        qualified(&schema_name, "session_summary")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("query session_summary view");
    assert_eq!(main_cost_cents, 20);
    assert_eq!(auxiliary_cost_cents, 6);

    let tool_model_tier: String = sqlx::query_scalar(&format!(
        "SELECT model_tier FROM {} WHERE session_id = $1 LIMIT 1",
        qualified(&schema_name, "tool_call_analytics")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("query tool_call_analytics view");
    assert_eq!(tool_model_tier, "main");

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn events_hot_path_triggers_are_removed_and_aggregates_maintained_by_app_db() {
    // Pins: the per-row `trg_update_session_aggregates` and
    // `events_set_tenant_columns` triggers were removed from the hot `events`
    // table; session aggregates are now maintained by the application append path.
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("no-trigger-ws", "test-model"))
        .await
        .expect("create session");

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let trigger_names: Vec<String> = sqlx::query_scalar(
        "SELECT t.tgname FROM pg_trigger t \
         JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname = 'events' AND n.nspname = $1 AND NOT t.tgisinternal",
    )
    .bind(&schema_name)
    .fetch_all(&pool)
    .await
    .expect("list events triggers");
    assert!(
        !trigger_names
            .iter()
            .any(|name| name == "trg_update_session_aggregates"),
        "aggregates trigger must be gone from the events hot path: {trigger_names:?}"
    );
    assert!(
        !trigger_names
            .iter()
            .any(|name| name == "events_set_tenant_columns"),
        "tenant-column trigger must be gone from the events hot path: {trigger_names:?}"
    );

    // The application append path still maintains session aggregates and binds
    // tenant_id explicitly (no BEFORE INSERT trigger needed).
    store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "hi".into(),
                thought_signature: None,
                model: ModelId::new("test-model"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 12,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 3,
                cost_cents: 5,
                duration_ms: 1,
                llm_ttft_ms: None,
            },
        )
        .await
        .expect("emit brain response");

    let row = sqlx::query(&format!(
        "SELECT event_count, total_cost_cents FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch session aggregates");
    use sqlx::Row as _;
    assert_eq!(row.get::<i64, _>("event_count"), 1);
    assert_eq!(row.get::<i64, _>("total_cost_cents"), 5);

    let event_tenant: Option<Uuid> = sqlx::query_scalar(&format!(
        "SELECT tenant_id FROM {} WHERE session_id = $1",
        qualified(&schema_name, "events")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch event tenant_id");
    assert!(
        event_tenant.is_some(),
        "append path must bind a non-null tenant_id without the trigger"
    );

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_tool_call_summary_view_reports_percentiles() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("tool-analytics-ws", "test-model"))
        .await
        .expect("create session");

    for (duration_ms, success) in [
        (100_u64, true),
        (200_u64, true),
        (300_u64, true),
        (400_u64, true),
        (500_u64, false),
    ] {
        let tool_id = ToolCallId::new();
        store
            .emit_event(
                session_id,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: serde_json::json!({ "cmd": "true" }),
                    hand_id: None,
                },
            )
            .await
            .expect("emit tool call");
        store
            .emit_event(
                session_id,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput::text("ok", Duration::from_millis(duration_ms)),
                    original_output_tokens: None,
                    success,
                    duration_ms,
                },
            )
            .await
            .expect("emit tool result");
    }

    let tenant_rows = store
        .list_tool_call_summaries(Some(&tenant_id("tool-analytics-ws")))
        .await
        .expect("load tenant tool summary");
    let summary = tenant_rows
        .iter()
        .find(|row| row.tool_name == "bash")
        .expect("bash summary");
    assert_eq!(summary.call_count, 5);
    assert!(approx_eq(summary.avg_duration_ms, 300.0, 1e-9));
    assert!(approx_eq(summary.p50_ms, 300.0, 1e-9));
    assert!(approx_eq(summary.p95_ms, 480.0, 1e-9));
    assert!(approx_eq(summary.success_rate, 0.8, 1e-9));

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let row: (i64, f64, f64) = sqlx::query_as(&format!(
        "SELECT call_count, p50_ms, p95_ms FROM {} WHERE tool_name = $1",
        qualified(&schema_name, "tool_call_summary")
    ))
    .bind("bash")
    .fetch_one(&pool)
    .await
    .expect("query tool_call_summary view");
    assert_eq!(row.0, 5);
    assert!(approx_eq(row.1, 300.0, 1e-9));
    assert!(approx_eq(row.2, 480.0, 1e-9));

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_materialized_analytics_views_refresh() {
    let (store, database_url, schema_name) = create_test_store().await;
    let storage_partition_id = workspace_key("mv-ws");
    let tenant_id = tenant_id("mv-ws");
    let first_session_id = store
        .create_session(test_session_meta("mv-ws", "test-model"))
        .await
        .expect("create first session");
    let second_session_id = store
        .create_session(test_session_meta("mv-ws", "test-model"))
        .await
        .expect("create second session");

    let tool_id = ToolCallId::new();
    store
        .emit_event(
            first_session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "file_read".to_string(),
                input: serde_json::json!({ "path": "README.md" }),
                hand_id: None,
            },
        )
        .await
        .expect("emit tool call");
    store
        .emit_event(
            first_session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text("ok", Duration::from_millis(120)),
                original_output_tokens: None,
                success: true,
                duration_ms: 120,
            },
        )
        .await
        .expect("emit tool result");
    for (session_id, llm_ms, uncached, cache_read, output, cost) in [
        (
            first_session_id,
            250_u64,
            15_usize,
            5_usize,
            4_usize,
            12_u32,
        ),
        (first_session_id, 175_u64, 8_usize, 2_usize, 3_usize, 6_u32),
        (
            second_session_id,
            300_u64,
            20_usize,
            10_usize,
            6_usize,
            18_u32,
        ),
    ] {
        store
            .emit_event(
                session_id,
                Event::BrainResponse {
                    text: "turn".to_string(),
                    thought_signature: None,
                    model: "test-model".into(),
                    model_tier: moa_core::types::provider::ModelTier::Main,
                    input_tokens_uncached: uncached,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: cache_read,
                    output_tokens: output,
                    cost_cents: cost,
                    duration_ms: llm_ms,
                    llm_ttft_ms: None,
                },
            )
            .await
            .expect("emit brain response");
    }

    store
        .refresh_analytics_materialized_views()
        .await
        .expect("refresh materialized analytics views");

    let turn_metrics = store
        .list_session_turn_metrics(first_session_id)
        .await
        .expect("load session turn metrics");
    assert_eq!(turn_metrics.len(), 2);
    assert_eq!(turn_metrics[0].turn_number, 1);
    assert!(approx_eq(turn_metrics[0].llm_ms, 250.0, 1e-9));
    assert!(approx_eq(turn_metrics[0].tool_ms, 120.0, 1e-9));
    assert_eq!(turn_metrics[0].tool_call_count, 1);
    assert_eq!(turn_metrics[0].total_input_tokens, 20);

    let tenant_summary = store
        .get_tenant_stats(&tenant_id, 30)
        .await
        .expect("load tenant stats");
    assert_eq!(tenant_summary.session_count, 2);
    assert_eq!(tenant_summary.turn_count, 3);
    assert_eq!(tenant_summary.total_input_tokens, 60);
    assert_eq!(tenant_summary.total_cache_read_tokens, 17);
    assert_eq!(tenant_summary.total_output_tokens, 13);
    assert_eq!(tenant_summary.total_cost_cents, 36);

    let daily_metrics = store
        .list_cache_daily_metrics(&tenant_id, 30)
        .await
        .expect("load cache daily metrics");
    assert_eq!(daily_metrics.len(), 1);
    assert_eq!(daily_metrics[0].session_count, 2);
    assert_eq!(daily_metrics[0].turn_count, 3);

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let session_count: i64 = sqlx::query_scalar(&format!(
        "SELECT session_count FROM {} WHERE storage_partition_id = $1",
        qualified(&schema_name, "daily_storage_partition_metrics")
    ))
    .bind(storage_partition_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("query daily workspace metrics");
    assert_eq!(session_count, 2);

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_analytics_query_read_models_refresh() {
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = tenant_id("analytics-facts");
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let contact_id = ContactId::new();
    let agent_id = Uuid::now_v7();
    let mut meta = contact_session_meta("analytics-facts", "test-model", contact_id);
    meta.agent_context.as_mut().expect("agent context").agent_id = Some(agent_id);
    let session_id = store.create_session(meta).await.expect("create session");
    let fixture_pool = PgPool::connect(&database_url)
        .await
        .expect("postgres fixture pool");
    sqlx::query(&format!(
        "INSERT INTO {} (id, tenant_id, storage_partition_id, state, display_name, contact_id) \
         VALUES ($1, $2, $3, 'verified', $4, $1)",
        qualified(&schema_name, "contacts")
    ))
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind("Analytics Contact")
    .execute(&fixture_pool)
    .await
    .expect("insert contact fixture");
    fixture_pool.close().await;

    SessionChannelStore::replace_session_channel_binding(
        &store,
        SessionChannelBindingUpdate {
            tenant_id,
            storage_partition_id,
            session_id,
            contact_id,
            channel_account_id: None,
            contact_point_id: None,
            channel_ref: ChannelRef::Slack {
                team_id: Some("T123".to_string()),
                slack_channel_id: Some("C123".to_string()),
                thread_ts: Some("1710000000.000100".to_string()),
                user_id: Some("U123".to_string()),
            },
            reason: Some("analytics fixture".to_string()),
        },
    )
    .await
    .expect("bind slack channel");

    let started_at = Utc::now();
    let segment_id = deterministic_segment_id(session_id, 0);
    store
        .create_segment(&TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: tenant_id.to_string(),
            segment_index: 0,
            task_summary: Some("Answer billing question".to_string()),
            started_at,
            ended_at: Some(started_at + chrono::Duration::milliseconds(1_500)),
            turn_count: 1,
            tools_used: vec!["file_read".to_string()],
            skills_activated: vec!["support-triage".to_string()],
            skills_used: vec!["support-triage".to_string()],
            token_cost: 22,
            previous_segment_id: None,
            outcome: Some(SegmentOutcome::Resolved.as_str().to_string()),
            assessment: None,
            outcome_confidence: Some(0.91),
        })
        .await
        .expect("create task segment");

    let tool_id = ToolCallId::new();
    store
        .emit_event(
            session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "file_read".to_string(),
                input: serde_json::json!({ "path": "README.md" }),
                hand_id: None,
            },
        )
        .await
        .expect("emit tool call");
    store
        .emit_event(
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text("ok", Duration::from_millis(120)),
                original_output_tokens: None,
                success: true,
                duration_ms: 120,
            },
        )
        .await
        .expect("emit tool result");
    store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "turn".to_string(),
                thought_signature: None,
                model: "test-model".into(),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 15,
                input_tokens_cache_write: 2,
                input_tokens_cache_read: 5,
                output_tokens: 4,
                cost_cents: 12,
                duration_ms: 250,
                llm_ttft_ms: None,
            },
        )
        .await
        .expect("emit brain response");

    let (execution_run_uid, execution_task_id, execution_skill_revision_uid) =
        seed_execution_analytics_rows(store.pool(), tenant_id.0, contact_id.0, session_id.0).await;

    store
        .refresh_analytics_materialized_views()
        .await
        .expect("refresh analytics query views");

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let session_row = sqlx::query(
        "SELECT tenant_id, agent_id, agent_display_name, channel, total_input_tokens, \
                total_cache_read_tokens, total_output_tokens, total_cost_cents \
         FROM analytics.session_fact WHERE session_id = $1",
    )
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("query analytics.session_fact");
    assert_eq!(session_row.get::<Uuid, _>("tenant_id"), tenant_id.0);
    assert_eq!(session_row.get::<Uuid, _>("agent_id"), agent_id);
    assert_eq!(
        session_row.get::<String, _>("agent_display_name"),
        "MOA Default Agent"
    );
    assert_eq!(session_row.get::<String, _>("channel"), "slack");
    assert_eq!(session_row.get::<i64, _>("total_input_tokens"), 22);
    assert_eq!(session_row.get::<i64, _>("total_cache_read_tokens"), 5);
    assert_eq!(session_row.get::<i64, _>("total_output_tokens"), 4);
    assert_eq!(session_row.get::<i64, _>("total_cost_cents"), 12);

    let turn_row = sqlx::query(
        "SELECT tenant_id, agent_id, channel, llm_ms, llm_ttft_ms, tool_ms, \
                tool_call_count, total_input_tokens, output_tokens, cost_cents \
         FROM analytics.turn_fact WHERE session_id = $1 AND turn_number = 1",
    )
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("query analytics.turn_fact");
    assert_eq!(turn_row.get::<Uuid, _>("tenant_id"), tenant_id.0);
    assert_eq!(turn_row.get::<Uuid, _>("agent_id"), agent_id);
    assert_eq!(turn_row.get::<String, _>("channel"), "slack");
    assert!(approx_eq(turn_row.get::<f64, _>("llm_ms"), 250.0, 1e-9));
    assert_eq!(turn_row.get::<Option<f64>, _>("llm_ttft_ms"), None);
    assert!(approx_eq(turn_row.get::<f64, _>("tool_ms"), 120.0, 1e-9));
    assert_eq!(turn_row.get::<i64, _>("tool_call_count"), 1);
    assert_eq!(turn_row.get::<i64, _>("total_input_tokens"), 22);
    assert_eq!(turn_row.get::<i64, _>("output_tokens"), 4);
    assert_eq!(turn_row.get::<i64, _>("cost_cents"), 12);

    let tool_row = sqlx::query(
        "SELECT tenant_id, agent_id, channel, tool_name, success, duration_ms, model_tier \
         FROM analytics.tool_call_fact WHERE session_id = $1 AND tool_id = $2",
    )
    .bind(session_id.0)
    .bind(tool_id.0)
    .fetch_one(&pool)
    .await
    .expect("query analytics.tool_call_fact");
    assert_eq!(tool_row.get::<Uuid, _>("tenant_id"), tenant_id.0);
    assert_eq!(tool_row.get::<Uuid, _>("agent_id"), agent_id);
    assert_eq!(tool_row.get::<String, _>("channel"), "slack");
    assert_eq!(tool_row.get::<String, _>("tool_name"), "file_read");
    assert!(tool_row.get::<bool, _>("success"));
    assert!(approx_eq(
        tool_row.get::<f64, _>("duration_ms"),
        120.0,
        1e-9
    ));
    assert_eq!(tool_row.get::<String, _>("model_tier"), "main");

    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM analytics.event_fact WHERE session_id = $1 AND tenant_id = $2",
    )
    .bind(session_id.0)
    .bind(tenant_id.0)
    .fetch_one(&pool)
    .await
    .expect("query analytics.event_fact");
    assert_eq!(event_count, 3);

    let segment_row = sqlx::query(
        "SELECT tenant_id, agent_id, channel, outcome, outcome_confidence, \
                tools_used, skills_activated, turn_count, token_cost, duration_ms \
         FROM analytics.task_segment_fact WHERE segment_id = $1",
    )
    .bind(segment_id.0)
    .fetch_one(&pool)
    .await
    .expect("query analytics.task_segment_fact");
    assert_eq!(segment_row.get::<Uuid, _>("tenant_id"), tenant_id.0);
    assert_eq!(segment_row.get::<Uuid, _>("agent_id"), agent_id);
    assert_eq!(segment_row.get::<String, _>("channel"), "slack");
    assert_eq!(segment_row.get::<String, _>("outcome"), "resolved");
    assert!(approx_eq(
        segment_row.get::<f64, _>("outcome_confidence"),
        0.91,
        1e-9
    ));
    assert_eq!(
        segment_row.get::<Vec<String>, _>("tools_used"),
        vec!["file_read".to_string()]
    );
    assert_eq!(
        segment_row.get::<Vec<String>, _>("skills_activated"),
        vec!["support-triage".to_string()]
    );
    assert_eq!(segment_row.get::<i32, _>("turn_count"), 1);
    assert_eq!(segment_row.get::<i64, _>("token_cost"), 22);
    assert!(approx_eq(
        segment_row.get::<f64, _>("duration_ms"),
        1_500.0,
        1e-9
    ));

    // Pins: V337's execution facts expose exact normalized values and bounded
    // failure metadata, without Task 9 aliases or raw error prose.
    let run_row = sqlx::query(
        "SELECT tenant_id, contact_id, session_id, initial_plan_hash, active_plan_hash, \
                plan_revision, source_kind, skill_template_ref, \
                skill_template_revision_uid, status, terminal_reason, requirement_count, \
                satisfied_requirement_count, completion_check_count, logical_task_count, \
                queue_to_start_ms, duration_ms, reserved_cost_microusd, actual_cost_microusd, \
                reserved_tokens, actual_tokens, reserved_tasks, actual_tasks, reserved_tool_calls, \
                actual_tool_calls, reserved_retrieved_bytes, actual_retrieved_bytes \
         FROM analytics.execution_run_fact WHERE run_uid = $1",
    )
    .bind(execution_run_uid)
    .fetch_one(&pool)
    .await
    .expect("query normalized execution run fact");
    assert_eq!(run_row.get::<Uuid, _>("tenant_id"), tenant_id.0);
    assert_eq!(run_row.get::<Uuid, _>("contact_id"), contact_id.0);
    assert_eq!(run_row.get::<Uuid, _>("session_id"), session_id.0);
    assert_eq!(
        run_row.get::<String, _>("initial_plan_hash"),
        "2".repeat(64)
    );
    assert_eq!(run_row.get::<String, _>("active_plan_hash"), "2".repeat(64));
    assert_eq!(run_row.get::<i64, _>("plan_revision"), 1);
    assert_eq!(run_row.get::<String, _>("source_kind"), "skill_template");
    assert_eq!(
        run_row.get::<String, _>("skill_template_ref"),
        "skill://billing-flow"
    );
    assert_eq!(
        run_row.get::<Uuid, _>("skill_template_revision_uid"),
        execution_skill_revision_uid
    );
    assert_eq!(run_row.get::<String, _>("status"), "completed");
    assert_eq!(run_row.get::<String, _>("terminal_reason"), "completed");
    assert_eq!(run_row.get::<i64, _>("requirement_count"), 2);
    assert_eq!(run_row.get::<i64, _>("satisfied_requirement_count"), 2);
    assert_eq!(run_row.get::<i64, _>("completion_check_count"), 1);
    assert_eq!(run_row.get::<i64, _>("logical_task_count"), 1);
    assert!(approx_eq(
        run_row.get::<f64, _>("queue_to_start_ms"),
        250.0,
        1e-9
    ));
    assert!(approx_eq(
        run_row.get::<f64, _>("duration_ms"),
        2_000.0,
        1e-9
    ));
    for (field, expected) in [
        ("reserved_cost_microusd", 0_i64),
        ("actual_cost_microusd", 125),
        ("reserved_tokens", 0),
        ("actual_tokens", 456),
        ("reserved_tasks", 0),
        ("actual_tasks", 1),
        ("reserved_tool_calls", 0),
        ("actual_tool_calls", 3),
        ("reserved_retrieved_bytes", 0),
        ("actual_retrieved_bytes", 4096),
    ] {
        assert_eq!(run_row.get::<i64, _>(field), expected, "{field}");
    }

    let task_row = sqlx::query(
        "SELECT tenant_id, run_uid, node_id, item_key, task_kind, capability_name, \
                capability_version, plan_revision, status, failure_class, attempt, generation, \
                citation_count, queue_latency_ms, duration_ms, reserved_cost_microusd, \
                actual_cost_microusd, reserved_tokens, actual_tokens, reserved_tasks, \
                actual_tasks, reserved_tool_calls, actual_tool_calls, reserved_retrieved_bytes, \
                actual_retrieved_bytes \
         FROM analytics.execution_task_fact WHERE task_id = $1",
    )
    .bind(execution_task_id)
    .fetch_one(&pool)
    .await
    .expect("query normalized execution task fact");
    assert_eq!(task_row.get::<Uuid, _>("tenant_id"), tenant_id.0);
    assert_eq!(task_row.get::<Uuid, _>("run_uid"), execution_run_uid);
    assert_eq!(task_row.get::<String, _>("node_id"), "lookup");
    assert_eq!(task_row.get::<String, _>("item_key"), "invoice-42");
    assert_eq!(task_row.get::<String, _>("task_kind"), "capability");
    assert_eq!(task_row.get::<String, _>("capability_name"), "docs.search");
    assert_eq!(task_row.get::<String, _>("capability_version"), "v2");
    assert_eq!(task_row.get::<i64, _>("plan_revision"), 1);
    assert_eq!(task_row.get::<String, _>("status"), "failed");
    assert_eq!(task_row.get::<String, _>("failure_class"), "invalid_output");
    assert_eq!(task_row.get::<i32, _>("attempt"), 2);
    assert_eq!(task_row.get::<i64, _>("generation"), 3);
    assert_eq!(task_row.get::<i64, _>("citation_count"), 2);
    assert!(approx_eq(
        task_row.get::<f64, _>("queue_latency_ms"),
        150.0,
        1e-9
    ));
    assert!(approx_eq(
        task_row.get::<f64, _>("duration_ms"),
        800.0,
        1e-9
    ));
    assert_eq!(task_row.get::<i64, _>("actual_cost_microusd"), 75);
    assert_eq!(task_row.get::<i64, _>("actual_tokens"), 110);
    assert_eq!(task_row.get::<i64, _>("actual_tasks"), 1);
    assert_eq!(task_row.get::<i64, _>("actual_tool_calls"), 2);
    assert_eq!(task_row.get::<i64, _>("actual_retrieved_bytes"), 900);

    for view_name in [
        "analytics.learning_candidate_fact",
        "analytics.experiment_run_fact",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {view_name}"))
            .fetch_one(&pool)
            .await
            .expect("query empty analytics fact view");
        assert_eq!(count, 0, "{view_name} should refresh while empty");
    }

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn append_events_batches_inserts_aggregates_and_dedupe_db() {
    // Pins: append_events inserts a batch in sequence order under one
    // transaction, folds session aggregates in the application (replacing the
    // retired per-row trigger), and honors per-entry dedupe keys.
    use sqlx::Row as _;

    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("tenant-batch", "test-model"))
        .await
        .expect("create session");

    let batch = vec![
        EventAppend {
            event: Event::UserMessage {
                text: "hello".into(),
                attachments: vec![],
            },
            dedupe_key: None,
        },
        EventAppend {
            event: Event::BrainResponse {
                text: "hi".into(),
                thought_signature: None,
                model: ModelId::new("test"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 10,
                input_tokens_cache_write: 2,
                input_tokens_cache_read: 3,
                output_tokens: 5,
                cost_cents: 7,
                duration_ms: 10,
                llm_ttft_ms: None,
            },
            dedupe_key: None,
        },
        EventAppend {
            event: Event::Checkpoint {
                summary: "sum".into(),
                events_summarized: 2,
                token_count: 4,
                model: ModelId::new("test"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens: 4,
                output_tokens: 6,
                cost_cents: 11,
            },
            dedupe_key: Some("dedupe-checkpoint".into()),
        },
    ];

    let records = store
        .append_events(session_id, batch)
        .await
        .expect("append batch");
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].sequence_num, 0);
    assert_eq!(records[1].sequence_num, 1);
    assert_eq!(records[2].sequence_num, 2);

    let events = store
        .get_events(
            session_id,
            moa_core::types::events_stream::EventRange::all(),
        )
        .await
        .expect("get events");
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].sequence_num, 0);
    assert!(matches!(events[0].event, Event::UserMessage { .. }));
    assert!(matches!(events[2].event, Event::Checkpoint { .. }));

    // Aggregate columns are maintained by the application, not a per-row trigger.
    let pool = PgPool::connect(&database_url)
        .await
        .expect("inspection pool");
    let row = sqlx::query(&format!(
        "SELECT event_count, turn_count, total_input_tokens, total_output_tokens, \
                total_cost_cents, last_checkpoint_seq \
         FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("load session aggregates");
    assert_eq!(row.get::<i64, _>("event_count"), 3);
    assert_eq!(row.get::<i64, _>("turn_count"), 1);
    assert_eq!(row.get::<i64, _>("total_input_tokens"), 19); // (10+2+3) + 4
    assert_eq!(row.get::<i64, _>("total_output_tokens"), 11); // 5 + 6
    assert_eq!(row.get::<i64, _>("total_cost_cents"), 18); // 7 + 11
    assert_eq!(row.get::<Option<i64>, _>("last_checkpoint_seq"), Some(2));

    // A repeated dedupe key short-circuits: no new event, same sequence returned,
    // aggregates unchanged.
    let deduped = store
        .append_events(
            session_id,
            vec![EventAppend {
                event: Event::Checkpoint {
                    summary: "again".into(),
                    events_summarized: 0,
                    token_count: 0,
                    model: ModelId::new("test"),
                    model_tier: moa_core::types::provider::ModelTier::Main,
                    input_tokens: 100,
                    output_tokens: 100,
                    cost_cents: 100,
                },
                dedupe_key: Some("dedupe-checkpoint".into()),
            }],
        )
        .await
        .expect("append deduped");
    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].sequence_num, 2);
    assert_eq!(deduped[0].id, records[2].id);
    assert!(matches!(
        &deduped[0].event,
        Event::Checkpoint {
            summary,
            input_tokens,
            ..
        } if summary == "sum" && *input_tokens == 4
    ));

    let after = store
        .get_events(
            session_id,
            moa_core::types::events_stream::EventRange::all(),
        )
        .await
        .expect("get events after dedupe");
    assert_eq!(after.len(), 3, "dedupe must not insert a new event");

    let cost_after: i64 = sqlx::query_scalar(&format!(
        "SELECT total_cost_cents FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("load cost after dedupe");
    assert_eq!(cost_after, 18, "deduped append must not change aggregates");

    let same_batch_duplicate = store
        .append_events(
            session_id,
            vec![
                EventAppend {
                    event: Event::UserMessage {
                        text: "first duplicate".into(),
                        attachments: vec![],
                    },
                    dedupe_key: Some("same-batch".into()),
                },
                EventAppend {
                    event: Event::Checkpoint {
                        summary: "should not be returned".into(),
                        events_summarized: 0,
                        token_count: 0,
                        model: ModelId::new("test"),
                        model_tier: moa_core::types::provider::ModelTier::Main,
                        input_tokens: 100,
                        output_tokens: 100,
                        cost_cents: 100,
                    },
                    dedupe_key: Some("same-batch".into()),
                },
            ],
        )
        .await
        .expect("append same-batch duplicate");
    assert_eq!(same_batch_duplicate.len(), 2);
    assert_eq!(same_batch_duplicate[0].id, same_batch_duplicate[1].id);
    assert_eq!(same_batch_duplicate[0].sequence_num, 3);
    assert_eq!(same_batch_duplicate[1].sequence_num, 3);
    assert_eq!(same_batch_duplicate[1].event_type, EventType::UserMessage);
    assert!(matches!(
        &same_batch_duplicate[1].event,
        Event::UserMessage { text, .. } if text == "first duplicate"
    ));

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

async fn seed_execution_analytics_rows(
    pool: &PgPool,
    tenant_id: Uuid,
    contact_id: Uuid,
    session_id: Uuid,
) -> (Uuid, Uuid, Uuid) {
    let run_uid = Uuid::now_v7();
    let task_id = Uuid::now_v7();
    let planning_context_uid = Uuid::now_v7();
    let skill_template_revision_uid = Uuid::now_v7();
    let planning_hash = "1".repeat(64);
    let plan_hash = "2".repeat(64);

    sqlx::query(
        "INSERT INTO moa.execution_planning_context \
             (planning_context_uid, tenant_id, contact_id, session_id, \
              originating_user_sequence_num, originating_user_event_hash, owner_user_id, \
              planning_context_hash, snapshot) \
         VALUES ($1, $2, $3, $4, 1, $5, 'analytics-user', $5, '{}'::JSONB)",
    )
    .bind(planning_context_uid)
    .bind(tenant_id)
    .bind(contact_id)
    .bind(session_id)
    .bind(&planning_hash)
    .execute(pool)
    .await
    .expect("insert execution planning context");
    sqlx::query(
        "INSERT INTO moa.execution_run \
             (run_uid, tenant_id, contact_id, session_id, originating_user_sequence_num, \
              planning_context_uid, planning_context_hash, owner_user_id, goal_contract, \
              initial_plan, active_plan, initial_plan_hash, active_plan_hash, capability_catalog, \
              authorization_envelope, source_provenance, source_kind, \
              skill_template_ref, skill_template_revision_uid, input, status, \
              progress_total_tasks) \
         VALUES ($1, $2, $3, $4, 1, $5, $6, 'analytics-user', \
                 '{\"requirements\":[{\"id\":\"r1\"},{\"id\":\"r2\"}], \
                   \"completion_checks\":[{\"id\":\"c1\"}]}'::JSONB, \
                 '{}'::JSONB, '{}'::JSONB, $7, $7, '{}'::JSONB, '{}'::JSONB, \
                 jsonb_build_object( \
                    'kind', 'skill_template', \
                    'skill_template_ref', 'skill://billing-flow', \
                    'skill_template_revision_uid', lower($8::TEXT)), \
                 'skill_template', \
                 'skill://billing-flow', $8, '{}'::JSONB, 'queued', 1)",
    )
    .bind(run_uid)
    .bind(tenant_id)
    .bind(contact_id)
    .bind(session_id)
    .bind(planning_context_uid)
    .bind(&planning_hash)
    .bind(&plan_hash)
    .bind(skill_template_revision_uid)
    .execute(pool)
    .await
    .expect("insert execution run");
    sqlx::query(
        "UPDATE moa.execution_run \
         SET status = 'running', started_at = queued_at + INTERVAL '250 milliseconds', \
             updated_at = queued_at + INTERVAL '250 milliseconds' \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(pool)
    .await
    .expect("start execution run");

    let task_created_at: chrono::DateTime<Utc> = sqlx::query_scalar(
        "SELECT started_at + INTERVAL '100 milliseconds' \
         FROM moa.execution_run WHERE run_uid = $1",
    )
    .bind(run_uid)
    .fetch_one(pool)
    .await
    .expect("read execution task fixture timestamp");
    sqlx::query(
        "INSERT INTO moa.execution_task \
             (task_id, run_uid, tenant_id, contact_id, node_id, item_key, plan_revision, status, \
              attempt, generation, input, task_kind, retry_policy, estimate_cost_microusd, \
              estimate_tokens, estimate_tasks, estimate_tool_calls, estimate_retrieved_bytes, \
              actual_cost_microusd, actual_tokens, actual_tasks, actual_tool_calls, \
              actual_retrieved_bytes, current_outcome, error, citations, created_at, started_at, \
              completed_at, updated_at) \
         VALUES ($1, $2, $3, $4, 'lookup', 'invoice-42', 1, 'failed', 2, 3, '{}'::JSONB, \
                 '{\"kind\":\"capability\", \
                   \"reference\":{\"name\":\"docs.search\",\"version\":\"v2\"}}'::JSONB, \
                 '{\"max_attempts\":2,\"initial_backoff_ms\":5,\"max_backoff_ms\":10}'::JSONB, \
                 80, 120, 1, 2, 1024, 75, 110, 1, 2, 900, \
                 '{\"class\":\"invalid_output\"}'::JSONB, \
                 '{\"class\":\"invalid_output\",\"message\":\"raw prose must not export\"}'::JSONB, \
                 '[{\"source\":\"doc-1\"},{\"source\":\"doc-2\"}]'::JSONB, \
                 $5, $5 + INTERVAL '150 milliseconds', \
                 $5 + INTERVAL '950 milliseconds', $5 + INTERVAL '950 milliseconds')",
    )
    .bind(task_id)
    .bind(run_uid)
    .bind(tenant_id)
    .bind(contact_id)
    .bind(task_created_at)
    .execute(pool)
    .await
    .expect("insert execution task");
    sqlx::query(
        "UPDATE moa.execution_run \
         SET status = 'completed', output = '{}'::JSONB, \
             completion_check_results = '[{\"check_id\":\"c1\"}]'::JSONB, \
             terminal_cause = '{\"kind\":\"completion\",\"limit_stop\":null}'::JSONB, \
             terminal_reason = 'completed', terminal_satisfied_requirement_count = 2, \
             terminal_requirement_count = 2, consumed_cost_microusd = 125, \
             consumed_tokens = 456, consumed_tasks = 1, consumed_tool_calls = 3, \
             consumed_retrieved_bytes = 4096, progress_completed_tasks = 1, \
             completed_at = started_at + INTERVAL '2 seconds', \
             updated_at = started_at + INTERVAL '2 seconds' \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(pool)
    .await
    .expect("complete execution run");

    (run_uid, task_id, skill_template_revision_uid)
}

fn approx_eq(left: f64, right: f64, epsilon: f64) -> bool {
    (left - right).abs() <= epsilon
}

#[tokio::test]
async fn analytics_mv_refresh_single_flights_under_advisory_lock_db() {
    // Pins: the analytics materialized-view refresh is single-flighted by a
    // Postgres advisory lock, so an overlapping run or replica that cannot take
    // the lease returns without doing work (never herds a concurrent rebuild).
    let (store, database_url, schema_name) = create_test_store().await;
    let key = store.analytics_mv_refresh_lock_key();

    let mut holder = store
        .pool()
        .acquire()
        .await
        .expect("acquire lease-holder connection");
    let mut contender = store
        .pool()
        .acquire()
        .await
        .expect("acquire contender connection");

    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(holder.as_mut())
        .await
        .expect("take the refresh lease");
    assert!(held, "the test should hold the refresh lease");

    let contended: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(contender.as_mut())
        .await
        .expect("contend for the held lease");
    assert!(!contended, "a second holder must not take the held lease");

    // The refresh single-flights out: it returns Ok without doing work or hanging.
    store
        .refresh_analytics_materialized_views()
        .await
        .expect("a refresh that cannot take the lease returns ok");

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(holder.as_mut())
        .await
        .expect("release the lease");

    let reacquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(contender.as_mut())
        .await
        .expect("reacquire the released lease");
    assert!(reacquired, "the lease is available once released");
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(contender.as_mut())
        .await
        .expect("release the reacquired lease");

    drop(holder);
    drop(contender);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
async fn analytics_mv_refresh_records_freshness_db() {
    // Pins: a completed refresh persists its success time and duration so the edge
    // can report read-model freshness without triggering work.
    let (store, database_url, schema_name) = create_test_store().await;

    store
        .refresh_analytics_materialized_views()
        .await
        .expect("refresh runs and records state");

    let (last_success, duration_ms): (Option<chrono::DateTime<Utc>>, Option<i64>) = sqlx::query_as(
        "SELECT last_success_at, last_duration_ms \
             FROM analytics.materialized_view_refresh_state WHERE id",
    )
    .fetch_one(store.pool())
    .await
    .expect("read the refresh state row");
    assert!(
        last_success.is_some(),
        "a completed refresh records a success time"
    );
    assert!(
        duration_ms.is_some(),
        "a completed refresh records its duration"
    );

    cleanup_schema(&database_url, &schema_name).await;
}

/// Builds a 1024-dim one-hot embedding for deterministic cosine-distance tests.
fn one_hot_embedding(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0_f32; 1024];
    vector[index] = 1.0;
    vector
}

/// Persists an assessed segment and its experience record with a task summary.
async fn seed_experience_with_summary(
    store: &PostgresSessionStore,
    session_id: SessionId,
    tenant: TenantId,
    index: u32,
    summary: &str,
    created_at: chrono::DateTime<Utc>,
) -> Uuid {
    let segment_id = deterministic_segment_id(session_id, index);
    let assessment = SegmentAssessment {
        outcome: SegmentOutcome::Resolved,
        confidence: 0.9,
        phase: AssessmentPhase::Immediate,
        evidence: Vec::new(),
        assessed_at: created_at,
        policy_version: "assessment-test".to_string(),
    };
    store
        .create_segment(&TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: tenant.to_string(),
            segment_index: index,
            task_summary: Some(summary.to_string()),
            started_at: created_at,
            ended_at: Some(created_at),
            turn_count: 2,
            tools_used: vec!["bash".to_string()],
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 10,
            previous_segment_id: None,
            outcome: Some("resolved".to_string()),
            assessment: Some(assessment.clone()),
            outcome_confidence: Some(assessment.confidence),
        })
        .await
        .expect("create segment");
    let experience = ExperienceRecord {
        id: Uuid::now_v7(),
        segment_id,
        session_id,
        tenant_id: tenant,
        user_id: UserId::new("user"),
        task_summary: Some(summary.to_string()),
        task_fingerprint: TaskFingerprint {
            hash: format!("hash-{index}"),
            normalized_summary: summary.to_string(),
            policy_version: "experience_v1".to_string(),
        },
        task_facets: TaskFacetSet::default(),
        actions: Vec::new(),
        resources: Vec::new(),
        outcome: SegmentOutcome::Resolved,
        confidence: 0.9,
        evidence: Vec::new(),
        tools_used: vec!["bash".to_string()],
        skills_activated: Vec::new(),
        skills_used: Vec::new(),
        turn_count: 2,
        token_cost: 10,
        duration_ms: Some(100),
        assessment_policy_version: assessment.policy_version.clone(),
        extraction_policy_version: "experience_v1".to_string(),
        created_at,
    };
    store
        .append_experience_record(&experience)
        .await
        .expect("append experience");
    experience.id
}

#[tokio::test]
#[ignore]
async fn experience_task_embedding_backfill_lists_sets_and_ranks_neighbors() {
    // Pins: the R2 embedding infrastructure round-trips end to end — missing rows
    // are selected newest-first within the lookback, a batch set clears them, and
    // the tenant nearest-neighbor primitive ranks by ascending cosine distance
    // and honors the self-exclusion.
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-task-embedding", "test-model"))
        .await
        .expect("create session");
    let tenant = tenant_id("pg-task-embedding");
    let now = Utc::now();
    let older = now - chrono::Duration::seconds(30);
    let alpha = seed_experience_with_summary(&store, session_id, tenant, 0, "alpha", older).await;
    let beta = seed_experience_with_summary(&store, session_id, tenant, 1, "beta", now).await;

    let missing = store
        .list_experience_records_missing_task_embedding(
            now - chrono::Duration::days(30),
            "mock-embedding-1024",
            1,
            10,
        )
        .await
        .expect("list missing");
    assert_eq!(
        missing.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![beta, alpha],
        "missing rows are returned newest-first",
    );
    assert_eq!(missing[0].task_summary, "beta");

    store
        .set_experience_task_embeddings(
            &[
                (alpha, "alpha".to_string(), one_hot_embedding(0)),
                (beta, "beta".to_string(), one_hot_embedding(1)),
            ],
            "mock-embedding-1024",
            1,
        )
        .await
        .expect("set embeddings");

    let after = store
        .list_experience_records_missing_task_embedding(
            now - chrono::Duration::days(30),
            "mock-embedding-1024",
            1,
            10,
        )
        .await
        .expect("list missing after set");
    assert!(after.is_empty(), "no rows remain missing after backfill");

    let neighbors = store
        .nearest_experience_task_embeddings(&tenant, &one_hot_embedding(0), 10, None)
        .await
        .expect("nearest neighbors");
    assert_eq!(
        neighbors.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![alpha, beta],
        "the probe's exact match ranks ahead of the orthogonal row",
    );
    assert!(neighbors[0].distance < 1e-3, "exact match has ~0 distance");
    assert!(
        neighbors[0].distance < neighbors[1].distance,
        "distances are ascending",
    );

    let excluded = store
        .nearest_experience_task_embeddings(&tenant, &one_hot_embedding(0), 10, Some(alpha))
        .await
        .expect("nearest neighbors excluding self");
    assert_eq!(
        excluded.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![beta],
        "the excluded id is dropped from the ranking",
    );

    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn experience_backfill_reselects_model_mismatched_rows() {
    // Pins: after an embedder switch, a row whose stored vector was produced by
    // the previous model is re-selected for embedding under the active model, so
    // incompatible vectors converge to one space instead of persisting forever.
    // Mutation guard: dropping the model/version predicate from the selection
    // query stops the row from re-selecting and fails the first assertion.
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-embed-mismatch", "test-model"))
        .await
        .expect("create session");
    let tenant = tenant_id("pg-embed-mismatch");
    let now = Utc::now();
    let exp = seed_experience_with_summary(&store, session_id, tenant, 0, "converge me", now).await;

    // Embed under the previous model.
    store
        .set_experience_task_embeddings(
            &[(exp, "converge me".to_string(), one_hot_embedding(0))],
            "old-model",
            1,
        )
        .await
        .expect("set old-model embedding");

    // Under the same model it is not stale...
    let same_model = store
        .list_experience_records_missing_task_embedding(
            now - chrono::Duration::days(30),
            "old-model",
            1,
            10,
        )
        .await
        .expect("list under same model");
    assert!(
        same_model.is_empty(),
        "a row already in the active space is not re-selected",
    );

    // ...but under a switched model (or version) it re-selects as needing re-embed.
    let switched_model = store
        .list_experience_records_missing_task_embedding(
            now - chrono::Duration::days(30),
            "new-model",
            1,
            10,
        )
        .await
        .expect("list under switched model");
    assert_eq!(
        switched_model.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![exp],
        "a model-mismatched row re-selects under the active model",
    );
    let switched_version = store
        .list_experience_records_missing_task_embedding(
            now - chrono::Duration::days(30),
            "old-model",
            2,
            10,
        )
        .await
        .expect("list under switched version");
    assert_eq!(
        switched_version
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![exp],
        "a version-mismatched row re-selects under the active model",
    );

    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn nearest_task_embeddings_for_experience_excludes_other_model_vectors() {
    // Pins: the recurrence-clustering neighbor lookup scopes to the
    // representative's own vector space, so a neighbor embedded by a different
    // model — even one whose bytes would rank identically — is never returned.
    // Mutation guard: dropping the model scope from the scoped NN query makes the
    // other-model row appear and fails the exclusion assertion.
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-embed-scope", "test-model"))
        .await
        .expect("create session");
    let tenant = tenant_id("pg-embed-scope");
    let now = Utc::now();
    let representative =
        seed_experience_with_summary(&store, session_id, tenant, 0, "repr", now).await;
    let same_space = seed_experience_with_summary(&store, session_id, tenant, 1, "same", now).await;
    let other_space =
        seed_experience_with_summary(&store, session_id, tenant, 2, "other", now).await;

    // Representative and same-space neighbor share model-x; the other-space row
    // has the identical direction but a different model.
    store
        .set_experience_task_embeddings(
            &[
                (representative, "repr".to_string(), one_hot_embedding(0)),
                (same_space, "same".to_string(), one_hot_embedding(0)),
            ],
            "model-x",
            1,
        )
        .await
        .expect("embed model-x rows");
    store
        .set_experience_task_embeddings(
            &[(other_space, "other".to_string(), one_hot_embedding(0))],
            "model-y",
            1,
        )
        .await
        .expect("embed model-y row");

    let neighbors = store
        .nearest_task_embeddings_for_experience(&tenant, representative, 10)
        .await
        .expect("neighbor lookup")
        .expect("representative is embedded");
    assert_eq!(
        neighbors.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![same_space],
        "only the same-model neighbor is returned; the other-model row is excluded",
    );

    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn nearest_experience_task_embeddings_scoped_filters_by_model() {
    // Pins: the public scoped primitive returns only rows in the requested vector
    // space when given Some(model), and every embedded row when given None — the
    // contract filing-time callers rely on to keep an active-model probe from
    // ranking against previous-space vectors.
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-embed-scoped-api", "test-model"))
        .await
        .expect("create session");
    let tenant = tenant_id("pg-embed-scoped-api");
    let now = Utc::now();
    let x = seed_experience_with_summary(&store, session_id, tenant, 0, "x", now).await;
    let y = seed_experience_with_summary(&store, session_id, tenant, 1, "y", now).await;
    store
        .set_experience_task_embeddings(&[(x, "x".to_string(), one_hot_embedding(0))], "model-x", 1)
        .await
        .expect("embed model-x row");
    store
        .set_experience_task_embeddings(&[(y, "y".to_string(), one_hot_embedding(0))], "model-y", 1)
        .await
        .expect("embed model-y row");

    let scoped = store
        .nearest_experience_task_embeddings_scoped(
            &tenant,
            &one_hot_embedding(0),
            10,
            None,
            Some(("model-x", 1)),
        )
        .await
        .expect("scoped neighbors");
    assert_eq!(
        scoped.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![x],
        "Some(model) returns only rows in that vector space",
    );

    let unscoped = store
        .nearest_experience_task_embeddings_scoped(&tenant, &one_hot_embedding(0), 10, None, None)
        .await
        .expect("unscoped neighbors");
    assert_eq!(
        unscoped.len(),
        2,
        "None compares against every embedded row regardless of model",
    );

    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn experience_embedding_write_refuses_summary_changed_under_it() {
    // Pins: a task summary that changed between the backfill's read and its write
    // does not get a vector of the stale text; and a re-assessment that rewrites
    // the summary clears any existing embedding so the new text is re-embedded.
    // Mutation guard: dropping `AND task_summary = $5` from the write persists the
    // stale vector and fails the "still missing" assertion.
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(test_session_meta("pg-embed-race", "test-model"))
        .await
        .expect("create session");
    let tenant = tenant_id("pg-embed-race");
    let now = Utc::now();

    let segment_id = deterministic_segment_id(session_id, 7);
    store
        .create_segment(&TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: tenant.to_string(),
            segment_index: 7,
            task_summary: Some("assess me".to_string()),
            started_at: now,
            ended_at: Some(now),
            turn_count: 2,
            tools_used: vec!["bash".to_string()],
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 10,
            previous_segment_id: None,
            outcome: Some("resolved".to_string()),
            assessment: None,
            outcome_confidence: Some(0.9),
        })
        .await
        .expect("create segment");
    let make_experience = |summary: &str| ExperienceRecord {
        id: Uuid::now_v7(),
        segment_id,
        session_id,
        tenant_id: tenant,
        user_id: UserId::new("user"),
        task_summary: Some(summary.to_string()),
        task_fingerprint: TaskFingerprint {
            hash: "race-hash".to_string(),
            normalized_summary: summary.to_string(),
            policy_version: "experience_v1".to_string(),
        },
        task_facets: TaskFacetSet::default(),
        actions: Vec::new(),
        resources: Vec::new(),
        outcome: SegmentOutcome::Resolved,
        confidence: 0.9,
        evidence: Vec::new(),
        tools_used: vec!["bash".to_string()],
        skills_activated: Vec::new(),
        skills_used: Vec::new(),
        turn_count: 2,
        token_cost: 10,
        duration_ms: Some(100),
        assessment_policy_version: "assessment_v1".to_string(),
        extraction_policy_version: "experience_v1".to_string(),
        created_at: now,
    };

    // Persist with the original summary, then re-assess (same segment + policy)
    // with a changed summary: the upsert keeps one row whose id is stable.
    let original = make_experience("old summary");
    let exp_id = original.id;
    store
        .append_experience_record(&original)
        .await
        .expect("persist original");
    let mut reassessed = make_experience("new summary");
    reassessed.id = exp_id;
    store
        .append_experience_record(&reassessed)
        .await
        .expect("re-assess with new summary");

    // The backfill "read" observed "old summary"; its write must be refused now
    // that the row holds "new summary".
    store
        .set_experience_task_embeddings(
            &[(exp_id, "old summary".to_string(), one_hot_embedding(0))],
            "test-model",
            1,
        )
        .await
        .expect("stale-summary write");
    let still_missing = store
        .list_experience_records_missing_task_embedding(
            now - chrono::Duration::days(30),
            "test-model",
            1,
            10,
        )
        .await
        .expect("list after stale write");
    assert!(
        still_missing.iter().any(|row| row.id == exp_id),
        "a write carrying the stale summary is refused; the row stays re-selectable",
    );

    // A write carrying the current summary applies and clears the row.
    store
        .set_experience_task_embeddings(
            &[(exp_id, "new summary".to_string(), one_hot_embedding(0))],
            "test-model",
            1,
        )
        .await
        .expect("current-summary write");
    let after_write = store
        .list_experience_records_missing_task_embedding(
            now - chrono::Duration::days(30),
            "test-model",
            1,
            10,
        )
        .await
        .expect("list after current write");
    assert!(
        !after_write.iter().any(|row| row.id == exp_id),
        "a write carrying the current summary embeds the row",
    );

    // Re-assessing with yet another summary clears the embedding so the new text
    // is re-embedded rather than stranding a vector of the old text.
    let mut reassessed_again = make_experience("newest summary");
    reassessed_again.id = exp_id;
    store
        .append_experience_record(&reassessed_again)
        .await
        .expect("re-assess again");
    let after_reassess = store
        .list_experience_records_missing_task_embedding(
            now - chrono::Duration::days(30),
            "test-model",
            1,
            10,
        )
        .await
        .expect("list after re-assess");
    assert!(
        after_reassess.iter().any(|row| row.id == exp_id),
        "a summary change on re-assessment clears the embedding for re-embed",
    );

    cleanup_schema(&database_url, &schema_name).await;
}
