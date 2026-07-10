//! Production-path DB coverage for agentic memory-tool admission.

use std::collections::BTreeSet;

use chrono::Utc;
use moa_core::{
    Channel, ContactId, ContactRef, ContactVerificationState, MoaConfig, ModelId, RlsContext,
    SessionId, SessionMeta, TenantId, ToolContent, ToolOutput,
};
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
    PostgresGraphStore,
};
use moa_orchestrator::services::memory::OrchestratorMemoryRetrievalExecutor;
use moa_session::testing;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn search_and_navigation_share_the_contact_memory_admission_boundary() {
    // Pins: agentic memory tools expose tenant knowledge and current-contact
    // memory only, and navigation cannot reveal a hidden seed or neighbor.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let other_contact_id = ContactId::new();
    let tenant_graph = graph_store(pool.clone(), RlsContext::tenant(tenant_id));
    let contact_graph = graph_store(pool.clone(), RlsContext::contact(tenant_id, contact_id));
    let other_contact_graph = graph_store(
        pool.clone(),
        RlsContext::contact(tenant_id, other_contact_id),
    );

    let tenant_chunk_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Chunk,
            "shared admission boundary answer",
            "tenant knowledge answer",
        ))
        .await
        .expect("create tenant knowledge chunk");
    let hidden_tenant_fact_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Fact,
            "shared admission boundary answer",
            "tenant operational answer must stay hidden",
        ))
        .await
        .expect("create tenant operational fact");
    let current_contact_fact_uid = contact_graph
        .create_node(node_intent(
            tenant_id,
            Some(contact_id),
            NodeLabel::Fact,
            "shared admission boundary answer",
            "current contact answer",
        ))
        .await
        .expect("create current-contact fact");
    let other_contact_fact_uid = other_contact_graph
        .create_node(node_intent(
            tenant_id,
            Some(other_contact_id),
            NodeLabel::Fact,
            "shared admission boundary answer",
            "other contact answer must stay hidden",
        ))
        .await
        .expect("create other-contact fact");
    tenant_graph
        .create_edge(EdgeWriteIntent {
            uid: Uuid::now_v7(),
            label: EdgeLabel::RelatesTo,
            start_uid: tenant_chunk_uid,
            end_uid: hidden_tenant_fact_uid,
            valid_from: Utc::now(),
            properties: json!({ "source": "memory-admission-test" }),
            storage_partition_id: Some(tenant_id.to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("create admitted-to-hidden edge");

    let executor = OrchestratorMemoryRetrievalExecutor;
    let session = contact_session(tenant_id, contact_id);
    let mut config = MoaConfig::default();
    config.memory.vector.embedder.name = "disabled".to_string();
    let search = executor
        .execute_retrieval_tool_with_runtime(
            &session,
            "memory_search",
            &json!({ "query": "shared admission boundary answer" }),
            &pool,
            &config,
        )
        .await
        .expect("execute memory search");
    assert!(!search.is_error, "{search:?}");
    assert_eq!(
        result_uids(&search, "hits", "graph_uid"),
        BTreeSet::from([tenant_chunk_uid, current_contact_fact_uid]),
        "search must admit exactly tenant knowledge and current-contact memory"
    );

    let navigate = executor
        .execute_retrieval_tool_with_runtime(
            &session,
            "memory_navigate",
            &json!({ "node_uid": tenant_chunk_uid, "hops": 1 }),
            &pool,
            &config,
        )
        .await
        .expect("navigate from admitted tenant chunk");
    assert!(!navigate.is_error, "{navigate:?}");
    assert_eq!(
        result_uids(&navigate, "neighbors", "uid"),
        BTreeSet::new(),
        "the hidden tenant-operational neighbor must be filtered"
    );

    let hidden_seed = executor
        .execute_retrieval_tool_with_runtime(
            &session,
            "memory_navigate",
            &json!({ "node_uid": hidden_tenant_fact_uid, "hops": 1 }),
            &pool,
            &config,
        )
        .await
        .expect("navigate from hidden seed");
    let missing_seed = executor
        .execute_retrieval_tool_with_runtime(
            &session,
            "memory_navigate",
            &json!({ "node_uid": Uuid::now_v7(), "hops": 1 }),
            &pool,
            &config,
        )
        .await
        .expect("navigate from missing seed");
    assert_eq!(tool_summary(&hidden_seed), tool_summary(&missing_seed));
    assert_eq!(
        result_uids(&hidden_seed, "neighbors", "uid"),
        result_uids(&missing_seed, "neighbors", "uid"),
        "hidden and missing seeds must have indistinguishable observable results"
    );

    let _ = tenant_graph
        .hard_purge(tenant_chunk_uid, "redacted:memory-admission-test")
        .await;
    let _ = tenant_graph
        .hard_purge(hidden_tenant_fact_uid, "redacted:memory-admission-test")
        .await;
    let _ = contact_graph
        .hard_purge(current_contact_fact_uid, "redacted:memory-admission-test")
        .await;
    let _ = other_contact_graph
        .hard_purge(other_contact_fact_uid, "redacted:memory-admission-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated database");
}

fn graph_store(pool: sqlx::PgPool, scope: RlsContext) -> PostgresGraphStore {
    PostgresGraphStore::scoped_for_app_role(pool, scope)
}

fn node_intent(
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    label: NodeLabel,
    name: &str,
    summary: &str,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        storage_partition_id: Some(tenant_id.to_string()),
        contact_id: contact_id.map(|id| id.to_string()),
        scope: if contact_id.is_some() {
            "contact".to_string()
        } else {
            "tenant".to_string()
        },
        name: name.to_string(),
        properties: json!({ "summary": summary }),
        pii_class: PiiClass::None,
        confidence: Some(0.95),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

fn contact_session(tenant_id: TenantId, contact_id: ContactId) -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        tenant_id,
        channel: Channel::Chat,
        model: ModelId::new("mock"),
        contact: Some(ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Verified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }),
        ..SessionMeta::default()
    }
}

fn result_uids(output: &ToolOutput, array_key: &str, uid_key: &str) -> BTreeSet<Uuid> {
    output
        .structured
        .as_ref()
        .and_then(|value| value.get(array_key))
        .and_then(Value::as_array)
        .expect("tool output should contain the expected result array")
        .iter()
        .map(|row| {
            row.get(uid_key)
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .expect("tool result should contain a UUID")
        })
        .collect()
}

fn tool_summary(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|content| match content {
            ToolContent::Text { text } => Some(text.as_str()),
            ToolContent::Json { .. } => None,
        })
        .expect("tool output should contain a text summary")
}
