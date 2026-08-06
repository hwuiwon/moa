//! Database coverage for session-pinned memory barriers and durable access audit.

use std::sync::Arc;

use moa_config::MoaConfig;
use moa_core::types::security::SensitivityClass;
use moa_core::{
    traits::{Identity, IdentityType, MemoryRetrievalExecutor},
    types::agent::{AgentContext, AgentKnowledgePolicy, AgentPolicySnapshot},
    types::contact::SessionActorRef,
    types::identifiers::{ModelId, SessionId, TenantId},
    types::memory::{InformationBarrierClearances, InformationBarrierId, RlsContext},
    types::session::SessionMeta,
};
use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_orchestrator::services::memory::OrchestratorMemoryRetrievalExecutor;
use moa_retrieval::engine::MemoryRetrievalEngine;
use moa_session::testing;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn pinned_clearances_isolate_reads_and_replayed_audit_is_idempotent_db_memory() {
    // Pins: retrieval derives information-barrier clearance only from the
    // pinned session policy, and one replay-stable tool operation produces one
    // exact signed access event even when another pod replays the call.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let kms: Arc<dyn moa_crypto::KeyManagementProvider> =
        Arc::new(moa_crypto::LocalKmsProvider::new());
    let tenant_id = TenantId::new();
    let barrier = InformationBarrierId::parse("matter-alpha").expect("valid barrier");
    let writer = PostgresGraphStore::scoped_for_app_role(
        pool.clone(),
        RlsContext::tenant(tenant_id).with_cleared_barriers(
            [barrier.clone()]
                .into_iter()
                .collect::<InformationBarrierClearances>(),
        ),
        kms.clone(),
    );
    let node_uid = writer
        .create_node(NodeWriteIntent {
            uid: Uuid::now_v7(),
            data_subject_id: tenant_id.0,
            label: NodeLabel::Chunk,
            storage_partition_id: Some(tenant_id.to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: "pinned clearance alpha evidence".to_string(),
            properties: json!({ "summary": "pinned clearance alpha evidence" }),
            pii_class: SensitivityClass::None,
            confidence: Some(0.95),
            valid_from: moa_test_support::fixtures::pg_now(),
            embedding: None,
            embedding_model: None,
            embedding_model_version: None,
            embedding_text: None,
            barrier: Some(barrier.clone()),
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("create barriered tenant knowledge node");

    let actor_id = Uuid::now_v7();
    let identity = Identity {
        identity_type: IdentityType::Agent,
        id: actor_id,
        tenant_id,
        api_key_id: Some(Uuid::now_v7()),
        acting_on_behalf_of: Some(Uuid::now_v7()),
    };
    let cleared = session_with_clearances(
        tenant_id,
        actor_id,
        [barrier].into_iter().collect(),
        "policy-cleared-v1",
    );
    let uncleared = session_with_clearances(
        tenant_id,
        actor_id,
        InformationBarrierClearances::new(),
        "policy-uncleared-v1",
    );
    let mut config = MoaConfig::default();
    config.memory.vector.embedder.name = "disabled".to_string();
    let retrieval_engine = Arc::new(
        MemoryRetrievalEngine::new(config, pool.clone(), kms.clone(), None)
            .with_assume_app_role(true),
    );
    let executor = OrchestratorMemoryRetrievalExecutor::from_retrieval_engine(
        pool.clone(),
        kms,
        retrieval_engine,
    );
    let operation_id = Uuid::now_v7().to_string();
    let audit_operation_id = format!("tool_call:{operation_id}");

    let cleared_output = executor
        .execute_retrieval_tool(
            &cleared,
            &identity,
            &operation_id,
            "memory_search",
            &json!({ "query": "pinned clearance alpha evidence" }),
        )
        .await
        .expect("execute cleared search");
    assert_eq!(
        hit_uids(cleared_output.structured_payload()),
        vec![node_uid]
    );

    let replay_output = executor
        .execute_retrieval_tool(
            &cleared,
            &identity,
            &operation_id,
            "memory_search",
            &json!({ "query": "pinned clearance alpha evidence" }),
        )
        .await
        .expect("replay cleared search from another worker");
    assert_eq!(hit_uids(replay_output.structured_payload()), vec![node_uid]);

    let uncleared_output = executor
        .execute_retrieval_tool(
            &uncleared,
            &identity,
            &format!("tool-call:{}", Uuid::now_v7()),
            "memory_search",
            &json!({ "query": "pinned clearance alpha evidence" }),
        )
        .await
        .expect("execute uncleared search");
    assert!(hit_uids(uncleared_output.structured_payload()).is_empty());

    let event_rows: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, event_jcs FROM security_events \
         WHERE tenant_id = $1 AND retrieval_operation_id = $2",
    )
    .bind(tenant_id.0)
    .bind(&audit_operation_id)
    .fetch_all(&pool)
    .await
    .expect("load replay-idempotent access event");
    assert_eq!(event_rows.len(), 1, "one logical retrieval emits one event");
    let payload: Value =
        serde_json::from_slice(&event_rows[0].1).expect("decode access event payload");
    assert_eq!(
        payload["actor"]["user"]["uid"],
        Value::from(format!("agent:{actor_id}"))
    );
    assert_eq!(
        payload["access"]["retrieval_operation_id"],
        Value::from(audit_operation_id)
    );
    assert_eq!(
        payload["access"]["node_uids"],
        json!([node_uid.to_string()])
    );

    let _ = writer
        .hard_purge(node_uid, "redacted:memory-service-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated database");
}

fn session_with_clearances(
    tenant_id: TenantId,
    actor_id: Uuid,
    cleared_barriers: InformationBarrierClearances,
    policy_hash: &str,
) -> SessionMeta {
    let mut agent_context = AgentContext::system_default();
    agent_context.policy_hash = policy_hash.to_string();
    agent_context.policy_snapshot = json!(AgentPolicySnapshot {
        knowledge_policy: AgentKnowledgePolicy {
            cleared_barriers,
            ..AgentKnowledgePolicy::default()
        },
        ..AgentPolicySnapshot::default()
    });
    SessionMeta {
        id: SessionId::new(),
        tenant_id,
        model: ModelId::new("mock"),
        created_by: Some(SessionActorRef::Identity { id: actor_id }),
        agent_context: Some(agent_context),
        ..SessionMeta::default()
    }
}

fn hit_uids(structured: Option<&Value>) -> Vec<Uuid> {
    structured
        .and_then(|value| value.get("hits"))
        .and_then(Value::as_array)
        .expect("memory search returns a hits array")
        .iter()
        .map(|hit| {
            hit.get("graph_uid")
                .and_then(Value::as_str)
                .and_then(|uid| Uuid::parse_str(uid).ok())
                .expect("memory hit carries a graph UUID")
        })
        .collect()
}
