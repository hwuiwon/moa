//! Postgres-backed checks for memory digest RLS and reader/writer schema drift.

use chrono::{TimeZone, Utc};
use moa_brain::pipeline::digest::DigestProcessor;
use moa_core::{
    config::MemoryDigestConfig, traits::ContextProcessor, types::channel::Channel,
    types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::context::ContextMessage,
    types::context::WorkingContext, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
    types::session::SessionMeta,
};
use moa_memory_lifecycle::rebuild_digests;
use moa_test_support::fixtures::{stable_uuid_from_label, tenant_id_from_storage_partition};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn digest_rls_blocks_cross_contact_and_cross_tenant_reads() {
    // Pins: app-role digest reads reveal only the caller's contact digest plus tenant digest.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let workspace_a = tenant_a.to_string();
    let workspace_b = tenant_b.to_string();
    let user_a = "digest-user-a";
    let user_b = "digest-user-b";
    let now = Utc
        .with_ymd_and_hms(2026, 6, 11, 0, 0, 0)
        .single()
        .expect("fixed timestamp");

    seed_fact(
        test_db.store().pool(),
        &workspace_a,
        Some(user_a),
        "editor theme",
        "prefers",
        "dark mode",
    )
    .await;
    seed_fact(
        test_db.store().pool(),
        &workspace_a,
        Some(user_b),
        "editor theme",
        "prefers",
        "light mode",
    )
    .await;
    seed_fact(
        test_db.store().pool(),
        &workspace_a,
        None,
        "deployment",
        "uses",
        "staging",
    )
    .await;
    seed_fact(
        test_db.store().pool(),
        &workspace_b,
        Some(user_a),
        "editor theme",
        "prefers",
        "solarized",
    )
    .await;

    let config = MemoryDigestConfig {
        enabled: true,
        max_tokens: 600,
        rebuild_min_interval_hours: 6,
    };
    rebuild_digests(test_db.store().pool(), &tenant_a, now, &config)
        .await
        .expect("rebuild tenant A digests");
    rebuild_digests(test_db.store().pool(), &tenant_b, now, &config)
        .await
        .expect("rebuild tenant B digests");

    let visible = visible_digest_contents(test_db.store().pool(), &workspace_a, user_a).await;

    assert_eq!(visible.len(), 2);
    assert!(visible.iter().any(|content| content.contains("dark mode")));
    assert!(visible.iter().any(|content| content.contains("staging")));
    assert!(!visible.iter().any(|content| content.contains("light mode")));
    assert!(!visible.iter().any(|content| content.contains("solarized")));

    cleanup_workspaces(test_db.store().pool(), &[&workspace_a, &workspace_b]).await;
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn digest_row_schema_pinned_across_writer_and_reader() {
    // Pins: lifecycle writer rows can be read and injected by the brain-local digest reader.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let user_id = "digest-reader-user";
    let now = Utc
        .with_ymd_and_hms(2026, 6, 11, 0, 0, 0)
        .single()
        .expect("fixed timestamp");

    seed_fact(
        test_db.store().pool(),
        &storage_partition_id,
        Some(user_id),
        "answer format",
        "prefers",
        "bullet points",
    )
    .await;
    seed_fact(
        test_db.store().pool(),
        &storage_partition_id,
        None,
        "release train",
        "uses",
        "weekly deploys",
    )
    .await;

    let config = MemoryDigestConfig {
        enabled: true,
        max_tokens: 600,
        rebuild_min_interval_hours: 6,
    };
    let stats = rebuild_digests(test_db.store().pool(), &tenant_id, now, &config)
        .await
        .expect("rebuild digests");
    assert_eq!(stats.digests_rebuilt, 2);

    let processor = DigestProcessor::new(test_db.store().pool().clone(), config);
    let mut ctx = working_context(&storage_partition_id, user_id);
    ctx.append_message(ContextMessage::user("What should I remember?"));

    let output = processor
        .process(&mut ctx)
        .await
        .expect("inject digest rows through brain reader");

    assert_eq!(
        output.items_included,
        vec!["digest:contact".to_string(), "digest:tenant".to_string()]
    );
    assert_eq!(ctx.messages.len(), 2);
    let digest_block = &ctx.messages[0].content;
    assert!(digest_block.contains("<memory_digest>"));
    assert!(digest_block.contains("bullet points"));
    assert!(digest_block.contains("weekly deploys"));
    assert!(
        digest_block.find("this user").expect("user digest")
            < digest_block.find("this tenant").expect("tenant digest")
    );

    let source_fact_count = sqlx::query_scalar::<_, i32>(
        "SELECT jsonb_array_length(source_fact_uids) FROM moa.memory_digests WHERE storage_partition_id = $1",
    )
    .bind(&storage_partition_id)
    .fetch_all(test_db.store().pool())
    .await
    .expect("read source uid counts")
    .into_iter()
    .sum::<i32>();
    assert_eq!(source_fact_count, 2);

    cleanup_workspaces(test_db.store().pool(), &[&storage_partition_id]).await;
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn digest_rebuild_deletes_identity_with_no_active_facts() {
    // Pins: a persisted digest whose facts are all inactive is deleted by rebuild
    // reconciliation, closing the erased-memory-in-prompt gap. Iterating only the
    // formed active-fact groups can never reach a zero-fact identity.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let user_id = "forgotten-contact";
    let now = Utc
        .with_ymd_and_hms(2026, 6, 11, 0, 0, 0)
        .single()
        .expect("fixed timestamp");

    seed_fact(
        test_db.store().pool(),
        &storage_partition_id,
        Some(user_id),
        "editor theme",
        "prefers",
        "dark mode",
    )
    .await;

    let config = MemoryDigestConfig {
        enabled: true,
        max_tokens: 600,
        rebuild_min_interval_hours: 6,
    };
    let stats = rebuild_digests(test_db.store().pool(), &tenant_id, now, &config)
        .await
        .expect("initial rebuild");
    assert_eq!(stats.digests_rebuilt, 1);
    assert_eq!(
        digest_row_count(test_db.store().pool(), &storage_partition_id).await,
        1
    );

    // Every active fact for the contact vanishes (erased or forgotten).
    sqlx::query("DELETE FROM moa.node_index WHERE storage_partition_id = $1")
        .bind(&storage_partition_id)
        .execute(test_db.store().pool())
        .await
        .expect("remove active facts");

    let later = now + chrono::Duration::hours(24);
    let stats = rebuild_digests(test_db.store().pool(), &tenant_id, later, &config)
        .await
        .expect("reconciling rebuild");
    assert_eq!(stats.digests_deleted, 1);
    assert_eq!(stats.digests_rebuilt, 0);
    assert_eq!(
        digest_row_count(test_db.store().pool(), &storage_partition_id).await,
        0
    );

    cleanup_workspaces(test_db.store().pool(), &[&storage_partition_id]).await;
}

async fn digest_row_count(pool: &PgPool, storage_partition_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.memory_digests WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id)
    .fetch_one(pool)
    .await
    .expect("count digest rows")
}

async fn seed_fact(
    pool: &PgPool,
    storage_partition_id: &str,
    user_id: Option<&str>,
    subject: &str,
    predicate: &str,
    object: &str,
) {
    let uid = Uuid::now_v7();
    let data_subject_id = Uuid::parse_str(storage_partition_id)
        .expect("digest storage partition fixture should be a tenant UUID");
    let name = format!("{subject} {predicate} {object}");
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, data_subject_id, user_id, name, pii_class, confidence, valid_from, properties_summary)
        VALUES ($1, 'Fact', $2, $3, $4, $5, 'none', 0.9, $6, $7)
        "#,
    )
    .bind(uid)
    .bind(storage_partition_id)
    .bind(data_subject_id)
    .bind(user_id)
    .bind(&name)
    .bind(
        Utc.with_ymd_and_hms(2026, 6, 10, 0, 0, 0)
            .single()
            .expect("fixed timestamp"),
    )
    .bind(json!({
        "subject": subject,
        "predicate": predicate,
        "object": object,
    }))
    .execute(pool)
    .await
    .expect("seed digest fact");
}

async fn visible_digest_contents(
    pool: &PgPool,
    storage_partition_id: &str,
    user_id: &str,
) -> Vec<String> {
    let mut tx = pool.begin().await.expect("begin app-role digest read");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tx)
        .await
        .expect("set app role");
    sqlx::query("SELECT pg_catalog.set_config('moa.storage_partition_id', $1, true)")
        .bind(storage_partition_id)
        .execute(&mut *tx)
        .await
        .expect("set workspace GUC");
    sqlx::query("SELECT pg_catalog.set_config('moa.user_id', $1, true)")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .expect("set user GUC");
    sqlx::query("SELECT pg_catalog.set_config('moa.scope_tier', 'user', true)")
        .execute(&mut *tx)
        .await
        .expect("set scope GUC");

    let rows = sqlx::query(
        "SELECT content FROM moa.memory_digests ORDER BY storage_partition_id, scope, user_id NULLS FIRST",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("read visible memory digests");
    tx.rollback().await.expect("rollback app-role digest read");

    rows.into_iter()
        .map(|row| row.try_get::<String, _>("content").expect("content"))
        .collect()
}

async fn cleanup_workspaces(pool: &PgPool, storage_partition_ids: &[&str]) {
    sqlx::query("DELETE FROM moa.memory_digests WHERE storage_partition_id = ANY($1)")
        .bind(storage_partition_ids)
        .execute(pool)
        .await
        .expect("cleanup digest rows");
    sqlx::query("DELETE FROM moa.node_index WHERE storage_partition_id = ANY($1)")
        .bind(storage_partition_ids)
        .execute(pool)
        .await
        .expect("cleanup node rows");
}

fn working_context(storage_partition_id: &str, user_id: &str) -> WorkingContext {
    let tenant_id = tenant_id_from_storage_partition(storage_partition_id);
    let contact_id = contact_id_from_user_id(user_id);
    WorkingContext::new(
        &SessionMeta {
            id: SessionId::new(),
            tenant_id,
            contact: Some(contact_ref(tenant_id, contact_id)),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        },
        capabilities(),
    )
}

fn contact_id_from_user_id(user_id: &str) -> ContactId {
    Uuid::parse_str(user_id)
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id)))
}

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    ContactRef {
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
    }
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("mock"),
        context_window: 32_000,
        max_output: 1_024,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: true,
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
