//! Postgres-backed checks for memory digest RLS and reader/writer schema drift.

use chrono::{TimeZone, Utc};
use moa_brain::pipeline::digest::DigestProcessor;
use moa_core::{
    Channel, ContactId, ContactRef, ContactVerificationState, ContextMessage, ContextProcessor,
    MemoryDigestConfig, ModelCapabilities, ModelId, SessionId, SessionMeta, TenantId, TokenPricing,
    ToolCallFormat, WorkingContext, WorkspaceId,
};
use moa_memory_lifecycle::digest::rebuild_digests;
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
async fn digest_rls_blocks_cross_user_and_cross_workspace_reads() {
    // Pins: app-role digest reads reveal only the caller's user digest plus workspace digest.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_a = format!("digest-rls-a-{}", Uuid::now_v7().simple());
    let workspace_b = format!("digest-rls-b-{}", Uuid::now_v7().simple());
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
    rebuild_digests(
        test_db.store().pool(),
        &WorkspaceId::new(&workspace_a),
        now,
        &config,
    )
    .await
    .expect("rebuild workspace A digests");
    rebuild_digests(
        test_db.store().pool(),
        &WorkspaceId::new(&workspace_b),
        now,
        &config,
    )
    .await
    .expect("rebuild workspace B digests");

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
    let workspace_id = format!("digest-schema-{}", Uuid::now_v7().simple());
    let user_id = "digest-reader-user";
    let now = Utc
        .with_ymd_and_hms(2026, 6, 11, 0, 0, 0)
        .single()
        .expect("fixed timestamp");

    seed_fact(
        test_db.store().pool(),
        &workspace_id,
        Some(user_id),
        "answer format",
        "prefers",
        "bullet points",
    )
    .await;
    seed_fact(
        test_db.store().pool(),
        &workspace_id,
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
    let stats = rebuild_digests(
        test_db.store().pool(),
        &WorkspaceId::new(&workspace_id),
        now,
        &config,
    )
    .await
    .expect("rebuild digests");
    assert_eq!(stats.digests_rebuilt, 2);

    let processor = DigestProcessor::new(test_db.store().pool().clone(), config);
    let mut ctx = working_context(&workspace_id, user_id);
    ctx.append_message(ContextMessage::user("What should I remember?"));

    let output = processor
        .process(&mut ctx)
        .await
        .expect("inject digest rows through brain reader");

    assert_eq!(
        output.items_included,
        vec!["digest:user".to_string(), "digest:workspace".to_string()]
    );
    assert_eq!(ctx.messages.len(), 2);
    let digest_block = &ctx.messages[0].content;
    assert!(digest_block.contains("<memory_digest>"));
    assert!(digest_block.contains("bullet points"));
    assert!(digest_block.contains("weekly deploys"));
    assert!(
        digest_block.find("this user").expect("user digest")
            < digest_block
                .find("this workspace")
                .expect("workspace digest")
    );

    let source_fact_count = sqlx::query_scalar::<_, i32>(
        "SELECT jsonb_array_length(source_fact_uids) FROM moa.memory_digests WHERE workspace_id = $1",
    )
    .bind(&workspace_id)
    .fetch_all(test_db.store().pool())
    .await
    .expect("read source uid counts")
    .into_iter()
    .sum::<i32>();
    assert_eq!(source_fact_count, 2);

    cleanup_workspaces(test_db.store().pool(), &[&workspace_id]).await;
}

async fn seed_fact(
    pool: &PgPool,
    workspace_id: &str,
    user_id: Option<&str>,
    subject: &str,
    predicate: &str,
    object: &str,
) {
    let uid = Uuid::now_v7();
    let name = format!("{subject} {predicate} {object}");
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, workspace_id, user_id, name, pii_class, confidence, valid_from, properties_summary)
        VALUES ($1, 'Fact', $2, $3, $4, 'none', 0.9, $5, $6)
        "#,
    )
    .bind(uid)
    .bind(workspace_id)
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

async fn visible_digest_contents(pool: &PgPool, workspace_id: &str, user_id: &str) -> Vec<String> {
    let mut tx = pool.begin().await.expect("begin app-role digest read");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tx)
        .await
        .expect("set app role");
    sqlx::query("SELECT pg_catalog.set_config('moa.workspace_id', $1, true)")
        .bind(workspace_id)
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
        "SELECT content FROM moa.memory_digests ORDER BY workspace_id, scope, user_id NULLS FIRST",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("read visible memory digests");
    tx.rollback().await.expect("rollback app-role digest read");

    rows.into_iter()
        .map(|row| row.try_get::<String, _>("content").expect("content"))
        .collect()
}

async fn cleanup_workspaces(pool: &PgPool, workspace_ids: &[&str]) {
    sqlx::query("DELETE FROM moa.memory_digests WHERE workspace_id = ANY($1)")
        .bind(workspace_ids)
        .execute(pool)
        .await
        .expect("cleanup digest rows");
    sqlx::query("DELETE FROM moa.node_index WHERE workspace_id = ANY($1)")
        .bind(workspace_ids)
        .execute(pool)
        .await
        .expect("cleanup node rows");
}

fn working_context(workspace_id: &str, user_id: &str) -> WorkingContext {
    let tenant_id = tenant_id_from_workspace_id(workspace_id);
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

fn tenant_id_from_workspace_id(workspace_id: &str) -> TenantId {
    Uuid::parse_str(workspace_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(workspace_id)))
}

fn contact_id_from_user_id(user_id: &str) -> ContactId {
    Uuid::parse_str(user_id)
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id)))
}

fn stable_uuid_from_label(label: &str) -> Uuid {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.as_bytes().iter().copied().enumerate() {
        let slot = index % 16;
        bytes[slot] = bytes[slot]
            .wrapping_mul(31)
            .wrapping_add(byte)
            .wrapping_add(index as u8);
        let mirror = (index * 7 + 3) % 16;
        bytes[mirror] ^= byte.rotate_left((index % 8) as u32);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
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
