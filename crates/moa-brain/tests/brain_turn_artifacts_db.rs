#[cfg(feature = "eval-harness")]
include!("brain_turn_support/common.rs");
#[cfg(feature = "eval-harness")]
include!("brain_turn_support/pipeline.rs");
#[cfg(feature = "eval-harness")]
include!("brain_turn_support/db.rs");
#[cfg(feature = "eval-harness")]
include!("brain_turn_support/artifacts.rs");

#[cfg(feature = "eval-harness")]
use moa_brain::BrainTurnRequest;

#[cfg(feature = "eval-harness")]
async fn allow_artifact_capture_bash(
    store: &moa_session::PostgresSessionStore,
    tenant_id: moa_core::types::identifiers::TenantId,
) {
    // These tests exercise artifact capture after local bash execution, so they
    // intentionally override the hardened AdminReview default for python fixtures.
    store
        .upsert_action_policy_rule(moa_core::types::action_policy::ActionPolicyRule {
            id: uuid::Uuid::now_v7(),
            scope: moa_core::types::action_policy::ActionRuleScope::Tenant { tenant_id },
            tool: "bash".to_string(),
            pattern: "python3 -c *".to_string(),
            effect: moa_core::types::action_policy::ActionPolicyEffect::Allow,
            reason: Some("artifact capture test bash opt-in".to_string()),
            created_by: moa_core::types::identifiers::UserId::new("artifact-capture-test"),
            created_at: moa_test_support::fixtures::pg_now(),
        })
        .await
        .expect("seed artifact test bash allow rule");
}

#[tokio::test]
async fn brain_turn_text_artifact_store_round_trips_db() {
    // Pins: the brain turn artifact DB lane exercises the real session-store blob path.
    use moa_core::{
        traits::SessionStore as _, types::contact::SessionActorRef, types::identifiers::ModelId,
        types::identifiers::TenantId, types::session::SessionMeta,
    };

    let (store, database_url, schema_name) = moa_session::testing::create_isolated_test_store()
        .await
        .expect("create isolated brain artifact DB store");
    let session_id = store
        .create_session(SessionMeta {
            tenant_id: TenantId::new(),
            model: ModelId::new("claude-sonnet-4-6"),
            created_by: Some(SessionActorRef::Identity {
                id: uuid::Uuid::now_v7(),
            }),
            ..SessionMeta::default()
        })
        .await
        .expect("create session for artifact storage");

    let claim = store
        .store_text_artifact(session_id, "artifact-backed tool output")
        .await
        .expect("store text artifact");
    let loaded = store
        .load_text_artifact(session_id, &claim)
        .await
        .expect("load text artifact");

    assert_eq!(loaded, "artifact-backed tool output");
    assert_eq!(claim.size, "artifact-backed tool output".len());
    assert!(claim.preview.contains("artifact-backed"));

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("cleanup isolated brain artifact DB schema");
}

#[cfg(feature = "eval-harness")]
#[tokio::test]
async fn run_brain_turn_uses_tool_result_search_for_artifact_backed_output() {
    let store = test_session_store().await;
    let session = SessionMeta {
        id: SessionId::new(),
        tenant_id: test_tenant_id(),
        contact: Some(test_contact_ref()),
        created_by: Some(SessionActorRef::Contact {
            id: test_contact_id(),
        }),
        model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    };
    store.create_session(session.clone()).await.unwrap();
    allow_artifact_capture_bash(&store, session.tenant_id).await;
    store
        .emit_event(
            session.id,
            Event::UserMessage {
                text: "Find bash-line-140 in a noisy command output".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    let sandbox_dir = tempdir().unwrap();
    let config = MoaConfig::default();
    let tool_router = Arc::new(
        ToolRouter::new_local(sandbox_dir.path())
            .await
            .unwrap()
            .with_policies(
                ActionPolicies::from_config(&config)
                    .expect("default test policy config should be valid"),
            )
            .with_rule_store(store.clone())
            .with_session_store(store.clone()),
    );
    let pipeline = build_no_memory_test_pipeline_with_tools(
        &config,
        store.clone(),
        tool_router.tool_schemas(),
    );
    let llm = Arc::new(ArtifactRetrievalLlmProvider::default());

    let result = run_brain_turn(BrainTurnRequest {
        identity: test_identity(session.tenant_id),
        session_id: session.id,
        session_store: store.clone(),
        llm_provider: llm.clone(),
        pipeline: &pipeline,
        tool_router: Some(tool_router),
    })
    .await
    .unwrap();

    assert_eq!(result, TurnResult::Complete);

    let events = store
        .get_events(session.id, EventRange::all())
        .await
        .unwrap();
    let bash_result = events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                output, success, ..
            } if *success && output.artifact.is_some() => Some(output.clone()),
            _ => None,
        })
        .expect("expected artifact-backed bash tool result");
    let artifact = bash_result
        .artifact
        .as_ref()
        .expect("artifact metadata should be present");
    assert!(artifact.estimated_tokens > 4_000);
    assert!(
        bash_result
            .to_text()
            .contains("full output stored separately"),
        "artifactized tool result should keep only a compact summary"
    );

    let search_result = events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                output,
                success,
                provider_tool_use_id: Some(provider_tool_use_id),
                ..
            } if *success && provider_tool_use_id == "44444444-4444-4444-4444-444444444444" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("expected tool_result_search output");
    assert!(search_result.to_text().contains("bash-line-140-"));
    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::BrainResponse { text, .. } if text == "Recovered bash-line-140 via tool_result_search"
    )));
}

#[cfg(feature = "eval-harness")]
#[tokio::test]
async fn run_brain_turn_reads_stderr_stream_from_artifact_backed_output() {
    let store = test_session_store().await;
    let session = SessionMeta {
        id: SessionId::new(),
        tenant_id: test_tenant_id(),
        contact: Some(test_contact_ref()),
        created_by: Some(SessionActorRef::Contact {
            id: test_contact_id(),
        }),
        model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    };
    store.create_session(session.clone()).await.unwrap();
    allow_artifact_capture_bash(&store, session.tenant_id).await;
    store
        .emit_event(
            session.id,
            Event::UserMessage {
                text: "Run the command and tell me what warning appeared on stderr".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .unwrap();

    let sandbox_dir = tempdir().unwrap();
    let config = MoaConfig::default();
    let tool_router = Arc::new(
        ToolRouter::new_local(sandbox_dir.path())
            .await
            .unwrap()
            .with_policies(
                ActionPolicies::from_config(&config)
                    .expect("default test policy config should be valid"),
            )
            .with_rule_store(store.clone())
            .with_session_store(store.clone()),
    );
    let pipeline = build_no_memory_test_pipeline_with_tools(
        &config,
        store.clone(),
        tool_router.tool_schemas(),
    );
    let llm = Arc::new(ArtifactStderrLlmProvider::default());

    let result = run_brain_turn(BrainTurnRequest {
        identity: test_identity(session.tenant_id),
        session_id: session.id,
        session_store: store.clone(),
        llm_provider: llm.clone(),
        pipeline: &pipeline,
        tool_router: Some(tool_router),
    })
    .await
    .unwrap();

    assert_eq!(result, TurnResult::Complete);

    let events = store
        .get_events(session.id, EventRange::all())
        .await
        .unwrap();
    let bash_result = events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                output, success, ..
            } if *success && output.artifact.is_some() => Some(output.clone()),
            _ => None,
        })
        .expect("expected artifact-backed bash tool result");
    let artifact = bash_result
        .artifact
        .as_ref()
        .expect("artifact metadata should be present");
    assert!(artifact.stderr_range.is_some());
    let stderr_read = events
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                output,
                success,
                provider_tool_use_id: Some(provider_tool_use_id),
                ..
            } if *success && provider_tool_use_id == "66666666-6666-6666-6666-666666666666" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("expected tool_result_read output");
    assert!(stderr_read.to_text().contains("warning: retrying fallback"));
    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::BrainResponse { text, .. } if text == "stderr warning recovered via tool_result_read"
    )));
}
