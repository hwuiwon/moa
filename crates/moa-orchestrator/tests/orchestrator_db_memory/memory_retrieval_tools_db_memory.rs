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
use moa_db::{ScopedConn, TENANT_WIDE_PRINCIPAL_HOLDER};
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_orchestrator::services::memory::OrchestratorMemoryRetrievalExecutor;
use moa_retrieval::engine::MemoryRetrievalEngine;
use moa_session::testing;
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn search_and_navigation_share_the_contact_memory_admission_boundary() {
    // Pins: agentic memory tools resolve provider-source principals once from
    // durable bindings, expose only bound governed tenant knowledge plus
    // current-contact memory, and navigation cannot reveal an unbound source,
    // hidden seed, or hidden neighbor.
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
    govern_tenant_chunk(&pool, tenant_id, tenant_chunk_uid, true).await;
    let unbound_tenant_chunk_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Chunk,
            "shared admission boundary answer",
            "unbound tenant knowledge must stay hidden",
        ))
        .await
        .expect("create unbound tenant knowledge chunk");
    govern_tenant_chunk(&pool, tenant_id, unbound_tenant_chunk_uid, false).await;
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
    // This test isolates admission. Production evidence-score abstention is
    // covered separately and must not discard a correctly admitted fixture row.
    config
        .memory
        .retrieval
        .ranking
        .abstain_below_window_evidence = 0.0;
    let retrieval_engine = Arc::new(
        MemoryRetrievalEngine::new(config, pool.clone(), kms.clone(), None)
            .with_assume_app_role(true),
    );
    let executor = OrchestratorMemoryRetrievalExecutor::from_retrieval_engine(
        pool.clone(),
        kms,
        retrieval_engine,
    );
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
    let unbound_seed = executor
        .execute_retrieval_tool(
            &session,
            &identity,
            "tool-call-unbound-seed",
            "memory_navigate",
            &json!({ "node_uid": unbound_tenant_chunk_uid, "hops": 1 }),
        )
        .await
        .expect("navigate from unbound governed seed");
    assert_eq!(
        tool_summary(&unbound_seed),
        tool_summary(&missing_seed),
        "an unbound source-ACL seed must be indistinguishable from a missing seed"
    );
    assert_eq!(
        result_uids(&unbound_seed, "neighbors", "uid"),
        result_uids(&missing_seed, "neighbors", "uid"),
        "an unbound source-ACL seed must not enter graph traversal"
    );

    let _ = tenant_graph
        .hard_purge(tenant_chunk_uid, "redacted:memory-admission-test")
        .await;
    let _ = tenant_graph
        .hard_purge(unbound_tenant_chunk_uid, "redacted:memory-admission-test")
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

async fn govern_tenant_chunk(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    chunk_uid: Uuid,
    bind_tenant_wide_principal: bool,
) {
    let partition = tenant_id.to_string();
    let connection_uid = Uuid::now_v7();
    let object_uid = Uuid::now_v7();
    let version_uid = Uuid::now_v7();
    let snapshot_uid = Uuid::now_v7();
    let mut principal_digest = [0_u8; 32];
    principal_digest[..16].copy_from_slice(chunk_uid.as_bytes());
    principal_digest[16..].copy_from_slice(chunk_uid.as_bytes());
    let principal =
        moa_core::types::memory::SourcePrincipalFingerprint::from_digest(1, principal_digest);
    let mut conn = ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true)
        .await
        .expect("begin source-ACL fixture transaction");
    sqlx::query(
        "INSERT INTO moa.connector_connections ( \
             connection_uid, tenant_id, display_name, built_in_key, built_in_version, \
             non_secret_config, lifecycle_status, health_status) \
         VALUES ($1, $2, 'memory-tool-test', 'knowledge:nango', 1, \
                 jsonb_build_object( \
                     'provider_config_key', 'memory-tool-test', \
                     'provider_connection_id', $1::TEXT, \
                     'connector', 'google-drive'), \
                 'active', 'ready')",
    )
    .bind(connection_uid)
    .bind(tenant_id.0)
    .execute(conn.as_mut())
    .await
    .expect("insert governed connector parent");
    sqlx::query(
        "INSERT INTO moa.knowledge_connections ( \
             connection_uid, tenant_id, storage_partition_id, provider, provider_config_key, \
             provider_connection_id, connector, metadata) \
         VALUES ($1, $2, $3, 'nango', 'memory-tool-test', $4, 'google-drive', '{}'::JSONB)",
    )
    .bind(connection_uid)
    .bind(tenant_id.0)
    .bind(&partition)
    .bind(connection_uid.to_string())
    .execute(conn.as_mut())
    .await
    .expect("insert governed connection");
    sqlx::query(
        "INSERT INTO moa.knowledge_objects ( \
             object_uid, tenant_id, storage_partition_id, connection_id, object_type, \
             external_object_id, status, metadata, acl_state) \
         VALUES ($1, $2, $3, $4, 'document', $5, 'active', '{}'::JSONB, 'incomplete')",
    )
    .bind(object_uid)
    .bind(tenant_id.0)
    .bind(&partition)
    .bind(connection_uid)
    .bind(object_uid.to_string())
    .execute(conn.as_mut())
    .await
    .expect("insert governed object");
    sqlx::query(
        "INSERT INTO moa.knowledge_document_versions ( \
             document_version_uid, tenant_id, storage_partition_id, object_id, \
             parser_provider, content_hash, metadata) \
         VALUES ($1, $2, $3, $4, 'native', $5, '{}'::JSONB)",
    )
    .bind(version_uid)
    .bind(tenant_id.0)
    .bind(&partition)
    .bind(object_uid)
    .bind(version_uid.to_string())
    .execute(conn.as_mut())
    .await
    .expect("insert governed document version");
    sqlx::query(
        "INSERT INTO moa.knowledge_chunks ( \
             chunk_uid, tenant_id, storage_partition_id, document_version_id, graph_node_uid, \
             chunk_hash, block_hashes, heading_path, text, ordinal, token_count, metadata) \
         VALUES ($1, $2, $3, $4, $1, $5, ARRAY[$5]::TEXT[], ARRAY[]::TEXT[], \
                 'tenant knowledge answer', 0, 3, '{}'::JSONB)",
    )
    .bind(chunk_uid)
    .bind(tenant_id.0)
    .bind(&partition)
    .bind(version_uid)
    .bind(chunk_uid.to_string())
    .execute(conn.as_mut())
    .await
    .expect("attach graph chunk to governed object");
    sqlx::query(
        "INSERT INTO moa.knowledge_source_acl_snapshots ( \
             snapshot_uid, tenant_id, storage_partition_id, connection_id, object_id, \
             provider_revision, snapshot_hash, complete, entry_count, captured_at) \
         VALUES ($1, $2, $3, $4, $5, 'rev-1', 'hash-1', TRUE, 1, now())",
    )
    .bind(snapshot_uid)
    .bind(tenant_id.0)
    .bind(&partition)
    .bind(connection_uid)
    .bind(object_uid)
    .execute(conn.as_mut())
    .await
    .expect("insert complete ACL snapshot");
    sqlx::query(
        "INSERT INTO moa.knowledge_source_acl_entries ( \
             entry_uid, tenant_id, storage_partition_id, snapshot_id, entry_kind, \
             principal_kind, principal_fingerprint, fingerprint_key_version) \
         VALUES ($1, $2, $3, $4, 'allow', 'anyone', $5, 1)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id.0)
    .bind(&partition)
    .bind(snapshot_uid)
    .bind(principal.as_bytes())
    .execute(conn.as_mut())
    .await
    .expect("insert Anyone allow entry");
    if bind_tenant_wide_principal {
        sqlx::query(
            "INSERT INTO moa.knowledge_source_principal_bindings ( \
                 binding_uid, tenant_id, storage_partition_id, contact_id, connection_id, \
                 principal_kind, principal_fingerprint, fingerprint_key_version, verified_at) \
             VALUES ($1, $2, $3, $4, $5, 'anyone', $6, 1, now())",
        )
        .bind(Uuid::now_v7())
        .bind(tenant_id.0)
        .bind(&partition)
        .bind(TENANT_WIDE_PRINCIPAL_HOLDER)
        .bind(connection_uid)
        .bind(principal.as_bytes())
        .execute(conn.as_mut())
        .await
        .expect("bind connection-scoped Anyone principal");
    }
    sqlx::query(
        "UPDATE moa.knowledge_objects \
            SET acl_state = 'current', acl_revision = 'rev-1', current_acl_snapshot_id = $2 \
          WHERE object_uid = $1",
    )
    .bind(object_uid)
    .bind(snapshot_uid)
    .execute(conn.as_mut())
    .await
    .expect("activate governed object ACL");
    conn.commit()
        .await
        .expect("commit source-ACL fixture transaction");
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
        .structured_payload()
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
            ToolContent::Json { .. } | ToolContent::Process { .. } => None,
        })
        .expect("tool output should contain a text summary")
}
