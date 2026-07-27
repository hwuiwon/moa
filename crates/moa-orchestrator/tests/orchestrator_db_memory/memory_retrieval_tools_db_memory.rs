//! Production-path DB coverage for agentic memory-tool admission.

use std::collections::BTreeSet;
use std::sync::Arc;

use moa_config::MoaConfig;
use moa_core::types::security::SensitivityClass;
use moa_core::{
    traits::Identity, traits::IdentityType, traits::MemoryRetrievalExecutor,
    types::channel::Channel, types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::contact::SessionActorRef,
    types::identifiers::ModelId, types::identifiers::SessionId, types::identifiers::TenantId,
    types::memory::RlsContext, types::session::SessionMeta, types::tools::ToolContent,
    types::tools::ToolOutput,
};
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore,
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
    let kms: Arc<dyn moa_crypto::KeyManagementProvider> =
        Arc::new(moa_crypto::LocalKmsProvider::new());
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let other_contact_id = ContactId::new();
    let tenant_graph = graph_store(pool.clone(), RlsContext::tenant(tenant_id), kms.clone());
    let contact_graph = graph_store(
        pool.clone(),
        RlsContext::contact(tenant_id, contact_id),
        kms.clone(),
    );
    let other_contact_graph = graph_store(
        pool.clone(),
        RlsContext::contact(tenant_id, other_contact_id),
        kms.clone(),
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
            valid_from: moa_test_support::fixtures::pg_now(),
            properties: json!({ "source": "memory-admission-test" }),
            storage_partition_id: Some(tenant_id.to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("create admitted-to-hidden edge");

    let session = contact_session(tenant_id, contact_id);
    let identity = Identity {
        identity_type: IdentityType::Contact,
        id: contact_id.0,
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let mut config = MoaConfig::default();
    config.memory.vector.embedder.name = "disabled".to_string();
    let executor = OrchestratorMemoryRetrievalExecutor::new(pool.clone(), kms, Arc::new(config));
    let search = executor
        .execute_retrieval_tool(
            &session,
            &identity,
            "tool-call-search",
            "memory_search",
            &json!({ "query": "shared admission boundary answer" }),
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
        .execute_retrieval_tool(
            &session,
            &identity,
            "tool-call-navigate",
            "memory_navigate",
            &json!({ "node_uid": tenant_chunk_uid, "hops": 1 }),
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
        .execute_retrieval_tool(
            &session,
            &identity,
            "tool-call-hidden-seed",
            "memory_navigate",
            &json!({ "node_uid": hidden_tenant_fact_uid, "hops": 1 }),
        )
        .await
        .expect("navigate from hidden seed");
    let missing_seed = executor
        .execute_retrieval_tool(
            &session,
            &identity,
            "tool-call-missing-seed",
            "memory_navigate",
            &json!({ "node_uid": Uuid::now_v7(), "hops": 1 }),
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

fn graph_store(
    pool: sqlx::PgPool,
    scope: RlsContext,
    kms: Arc<dyn moa_crypto::KeyManagementProvider>,
) -> PostgresGraphStore {
    PostgresGraphStore::scoped_for_app_role(pool, scope, kms)
}

fn node_intent(
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    label: NodeLabel,
    name: &str,
    summary: &str,
) -> NodeWriteIntent {
    NodeWriteIntent {
        barrier: None,
        uid: Uuid::now_v7(),
        data_subject_id: contact_id.map_or(tenant_id.0, |contact_id| contact_id.0),
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
        pii_class: SensitivityClass::None,
        confidence: Some(0.95),
        valid_from: moa_test_support::fixtures::pg_now(),
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
        created_by: Some(SessionActorRef::Contact { id: contact_id }),
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
