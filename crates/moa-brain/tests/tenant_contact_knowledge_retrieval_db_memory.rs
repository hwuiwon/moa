//! Integration coverage for tenant knowledge plus contact-memory retrieval.

use chrono::Utc;
use moa_brain::pipeline::memory::GraphMemoryRetriever;
use moa_core::{
    Channel, ContactId, ContactRef, ContactVerificationState, ContextMessage, ContextProcessor,
    ModelCapabilities, ModelId, SessionId, SessionMeta, TenantId, TokenPricing, ToolCallFormat,
};
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_memory_types::ScopeContext;
use moa_session::testing;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn mock_tenant_and_contact_retrieval() {
    // Pins: tenant KB chunks and the current contact's memory are retrieved together.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let other_contact_id = ContactId::new();
    let tenant_scope = ScopeContext::tenant(tenant_id);
    let contact_scope = ScopeContext::contact(tenant_id, contact_id);
    let other_contact_scope = ScopeContext::contact(tenant_id, other_contact_id);
    let tenant_graph = AgeGraphStore::scoped_for_app_role(pool.clone(), tenant_scope.clone());
    let contact_graph = AgeGraphStore::scoped_for_app_role(pool.clone(), contact_scope);
    let other_contact_graph = AgeGraphStore::scoped_for_app_role(pool.clone(), other_contact_scope);

    let chunk_graph_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Chunk,
            "tenant pto runbook answer",
            json!({ "summary": "tenant node summary should be replaced by hydrated chunk text" }),
        ))
        .await
        .expect("create tenant chunk graph node");
    seed_knowledge_chunk(&pool, tenant_id, chunk_graph_uid)
        .await
        .expect("seed tenant knowledge rows");
    let contact_fact_uid = contact_graph
        .create_node(node_intent(
            tenant_id,
            Some(contact_id),
            NodeLabel::Fact,
            "contact deployment preference answer",
            json!({ "summary": "current contact prefers blue deployment windows" }),
        ))
        .await
        .expect("create current contact fact");
    let other_contact_fact_uid = other_contact_graph
        .create_node(node_intent(
            tenant_id,
            Some(other_contact_id),
            NodeLabel::Fact,
            "contact deployment preference answer",
            json!({ "summary": "other contact private memory should not appear" }),
        ))
        .await
        .expect("create other contact fact");

    let session = contact_session(tenant_id, contact_id);
    let mut ctx = moa_core::WorkingContext::new(&session, capabilities());
    ctx.append_message(ContextMessage::user(
        "Find the pto runbook answer and contact deployment preference answer",
    ));
    let retriever = GraphMemoryRetriever::new(pool.clone(), None)
        .with_assume_app_role(true)
        .with_result_limit(6);

    let output = retriever
        .process(&mut ctx)
        .await
        .expect("graph memory retrieval should assemble context");

    assert!(
        output
            .items_included
            .iter()
            .any(|item| item == &format!("graph:Chunk:{chunk_graph_uid}")),
        "{:?}",
        output.items_included
    );
    assert!(
        output
            .items_included
            .iter()
            .any(|item| item == &format!("graph:Fact:{contact_fact_uid}")),
        "{:?}",
        output.items_included
    );
    assert!(
        output
            .items_included
            .iter()
            .all(|item| item != &format!("graph:Fact:{other_contact_fact_uid}")),
        "{:?}",
        output.items_included
    );
    let memory_message = ctx
        .messages
        .first()
        .expect("memory reminder should be inserted");
    assert!(memory_message.content.contains("<knowledge_context>"));
    assert!(memory_message.content.contains("<tenant_knowledge>"));
    assert!(memory_message.content.contains("<user_memory>"));
    let tenant_section = section_between(
        &memory_message.content,
        "<tenant_knowledge>",
        "</tenant_knowledge>",
    );
    let user_section = section_between(&memory_message.content, "<user_memory>", "</user_memory>");
    assert!(tenant_section.contains("Hydrated tenant PTO policy chunk text"));
    assert!(tenant_section.contains("source_uri=https://example.test/pto-runbook"));
    assert!(!tenant_section.contains("tenant node summary should be replaced"));
    assert!(user_section.contains("current contact prefers blue deployment windows"));
    assert!(
        !memory_message
            .content
            .contains("other contact private memory")
    );

    let _ = tenant_graph
        .hard_purge(chunk_graph_uid, "redacted:tenant-contact-knowledge-test")
        .await;
    let _ = contact_graph
        .hard_purge(contact_fact_uid, "redacted:tenant-contact-knowledge-test")
        .await;
    let _ = other_contact_graph
        .hard_purge(
            other_contact_fact_uid,
            "redacted:tenant-contact-knowledge-test",
        )
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

fn node_intent(
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    label: NodeLabel,
    name: &str,
    properties: serde_json::Value,
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
        properties,
        pii_class: PiiClass::None,
        confidence: Some(0.95),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn seed_knowledge_chunk(
    pool: &PgPool,
    tenant_id: TenantId,
    graph_node_uid: Uuid,
) -> sqlx::Result<()> {
    let connection_uid = Uuid::now_v7();
    let object_uid = Uuid::now_v7();
    let version_uid = Uuid::now_v7();
    let chunk_uid = Uuid::now_v7();
    let storage_partition_id = tenant_id.to_string();
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_connections (
            connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
            provider_connection_id, connector, credential_ref, status, metadata
        )
        VALUES ($1, $2, $3, 'merge', 'test-config', 'acct-tenant-contact', 'drive',
                'vault://tenant-contact-test', 'active', '{}'::jsonb)
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_objects (
            object_uid, tenant_id, storage_partition_id, connection_id, object_type,
            external_object_id, title, change_token, source_uri, status, metadata
        )
        VALUES ($1, $2, $3, $4, 'document', 'pto-runbook', 'PTO Runbook',
                'etag-1', 'https://example.test/pto-runbook', 'active', '{}'::jsonb)
        "#,
    )
    .bind(object_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(connection_uid)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_document_versions (
            document_version_uid, tenant_id, storage_partition_id, object_id,
            parser_provider, parser_job_id, content_hash, metadata
        )
        VALUES ($1, $2, $3, $4, 'native', 'native-job-1', 'content-hash-1', '{}'::jsonb)
        "#,
    )
    .bind(version_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(object_uid)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_chunks (
            chunk_uid, tenant_id, storage_partition_id, document_version_id, graph_node_uid,
            chunk_hash, block_hashes, heading_path, text, ordinal, token_count, metadata
        )
        VALUES ($1, $2, $3, $4, $5, 'chunk-hash-1', ARRAY['block-hash-1']::text[],
                ARRAY['People', 'PTO']::text[],
                'Hydrated tenant PTO policy chunk text. The runbook answer is owner approval.',
                0, 13, '{}'::jsonb)
        "#,
    )
    .bind(chunk_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(version_uid)
    .bind(graph_node_uid)
    .execute(pool)
    .await?;
    Ok(())
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
            permissions: serde_json::Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }),
        ..SessionMeta::default()
    }
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("mock"),
        context_window: 32_000,
        max_output: 1_024,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: false,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 1.0,
            output_per_mtok: 1.0,
            cached_input_per_mtok: None,
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        native_tools: Vec::new(),
    }
}

fn section_between<'a>(content: &'a str, start: &str, end: &str) -> &'a str {
    let start_index = content
        .find(start)
        .expect("section start marker should exist")
        + start.len();
    let end_index = content[start_index..]
        .find(end)
        .expect("section end marker should exist")
        + start_index;
    &content[start_index..end_index]
}
