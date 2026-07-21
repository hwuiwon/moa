//! Live-Postgres coverage for batched KMS use and root-key rotation.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_crypto::{
    DataKeyDecryptRequest, EncryptionContext, Error as CryptoError, KeyManagementProvider,
};
use moa_db::ScopedConn;
use moa_kms::{KmsError, PostgresKmsProvider, ROOT_KEY_LEN, RootKeyRing};
use moa_session::testing;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn keyring(required: &str, generations: &[(&str, u8)]) -> RootKeyRing {
    RootKeyRing::from_directory_entries(
        PathBuf::from("/var/run/secrets/moa-kms/root-keys"),
        required,
        generations.iter().map(|(generation, seed)| {
            (
                (*generation).to_string(),
                BASE64.encode([*seed; ROOT_KEY_LEN]),
            )
        }),
    )
    .expect("build test keyring")
}

async fn connect(url: &str, max: u32) -> PgPool {
    PgPoolOptions::new()
        .max_connections(max)
        .connect(url)
        .await
        .expect("connect isolated test database")
}

async fn count_kek_rows(pool: &PgPool, tenant: Uuid, subject: Uuid) -> i64 {
    let mut conn =
        ScopedConn::begin_as_app(pool, &RlsContext::tenant(TenantId::from(tenant)), true)
            .await
            .expect("begin tenant count");
    let count =
        sqlx::query_scalar("SELECT count(*) FROM moa.kek WHERE tenant_id = $1 AND subject_id = $2")
            .bind(tenant)
            .bind(subject)
            .fetch_one(conn.as_mut())
            .await
            .expect("count KEKs");
    conn.commit().await.expect("commit count");
    count
}

async fn live_generation_versions(pool: &PgPool) -> Vec<(String, i64)> {
    let mut conn = ScopedConn::begin_control_plane(pool)
        .await
        .expect("begin control-plane query");
    conn.assume_app_role().await.expect("assume app role");
    let rows = sqlx::query_as(
        "SELECT root_key_generation, rewrap_version FROM moa.kek WHERE destroyed_at IS NULL ORDER BY kek_id",
    )
    .fetch_all(conn.as_mut())
    .await
    .expect("read live generation versions");
    conn.commit().await.expect("commit generation query");
    rows
}

#[tokio::test]
async fn activation_mixed_generation_readiness_retirement_and_restart_db() {
    // Pins: activation changes only new writes; historical KEKs remain readable
    // through their recorded generation, block retirement and missing-key
    // readiness, then become restart-safe after bounded rewrap.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = connect(&database_url, 8).await;
    let tenant = Uuid::now_v7();
    let old_ctx = EncryptionContext::new(tenant, Uuid::now_v7(), "old-record", "restricted");
    let new_ctx = EncryptionContext::new(tenant, Uuid::now_v7(), "new-record", "restricted");

    let primary = PostgresKmsProvider::new(
        pool.clone(),
        keyring("primary", &[("primary", 1), ("next", 2)]),
    );
    let old = primary
        .generate_data_key(&old_ctx)
        .await
        .expect("generate under primary");
    let state = primary
        .activate_generation("next")
        .await
        .expect("activate next");
    assert_eq!(state.active_generation, "next");
    assert_eq!(state.state_version, 1);
    assert!(matches!(
        primary.check_compatibility().await,
        Err(KmsError::RequiredGenerationInactive { active, required })
            if active == "next" && required == "primary"
    ));

    let missing_historical =
        PostgresKmsProvider::new(pool.clone(), keyring("next", &[("next", 2)]));
    let readiness = missing_historical
        .check_compatibility()
        .await
        .expect_err("live primary KEK requires its historical root key");
    assert!(matches!(
        readiness,
        KmsError::RootKeyGenerationMissing(generation) if generation == "primary"
    ));

    let next = PostgresKmsProvider::new(
        pool.clone(),
        keyring("next", &[("primary", 1), ("next", 2)]),
    );
    next.check_compatibility()
        .await
        .expect("complete keyring is compatible");
    let new = next
        .generate_data_key(&new_ctx)
        .await
        .expect("generate under next");
    next.decrypt_data_key(&old.wrapped, &old.handle, &old_ctx)
        .await
        .expect("historical generation decrypts before rewrap");
    next.decrypt_data_key(&new.wrapped, &new.handle, &new_ctx)
        .await
        .expect("active generation decrypts");

    let retirement = next
        .retire_generation("primary")
        .await
        .expect_err("referenced generation must not retire");
    assert!(matches!(
        retirement,
        KmsError::RootKeyGenerationReferenced {
            generation,
            references: 1
        } if generation == "primary"
    ));
    assert_eq!(next.rewrap_batch(10).await.expect("rewrap"), 1);
    assert_eq!(next.rewrap_batch(10).await.expect("rewrap complete"), 0);
    next.retire_generation("primary")
        .await
        .expect("unreferenced historical generation retires");

    drop(next);
    let restarted = PostgresKmsProvider::new(pool.clone(), keyring("next", &[("next", 2)]));
    restarted
        .check_compatibility()
        .await
        .expect("retired key can be unmounted after rewrap");
    let recovered = restarted
        .decrypt_data_key(&old.wrapped, &old.handle, &old_ctx)
        .await
        .expect("rewrapped KEK decrypts after restart");
    assert_eq!(recovered.expose(), old.plaintext.expose());

    pool.close().await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated database");
}

#[tokio::test]
async fn concurrent_bounded_rewrap_jobs_claim_each_kek_once_db() {
    // Pins: concurrent jobs use SKIP LOCKED and generation/version CAS so every
    // historical KEK is moved once, with no pod-local ownership or duplicate work.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = connect(&database_url, 16).await;
    let tenant = Uuid::now_v7();
    let primary = PostgresKmsProvider::new(
        pool.clone(),
        keyring("primary", &[("primary", 3), ("next", 4)]),
    );
    let mut records = Vec::new();
    for index in 0..12_u32 {
        let context = EncryptionContext::new(
            tenant,
            Uuid::now_v7(),
            format!("record-{index}"),
            "restricted",
        );
        let generated = primary
            .generate_data_key(&context)
            .await
            .expect("generate historical key");
        records.push((context, generated));
    }
    primary
        .activate_generation("next")
        .await
        .expect("activate next");

    let worker_a = Arc::new(PostgresKmsProvider::new(
        pool.clone(),
        keyring("next", &[("primary", 3), ("next", 4)]),
    ));
    let worker_b = Arc::new(PostgresKmsProvider::new(
        pool.clone(),
        keyring("next", &[("primary", 3), ("next", 4)]),
    ));
    let run = |provider: Arc<PostgresKmsProvider>| {
        tokio::spawn(async move {
            let mut total = 0_u64;
            loop {
                let claimed = provider.rewrap_batch(2).await.expect("rewrap batch");
                total += claimed;
                if claimed == 0 {
                    return total;
                }
                tokio::task::yield_now().await;
            }
        })
    };
    let (a, b) = tokio::join!(run(worker_a.clone()), run(worker_b));
    assert_eq!(
        a.expect("join rewrap A") + b.expect("join rewrap B"),
        records.len() as u64
    );
    let versions = live_generation_versions(&pool).await;
    assert_eq!(versions.len(), records.len());
    assert!(
        versions
            .iter()
            .all(|(generation, version)| generation == "next" && *version == 1)
    );
    for (context, generated) in records {
        worker_a
            .decrypt_data_key(&generated.wrapped, &generated.handle, &context)
            .await
            .expect("rewrapped record decrypts");
    }

    pool.close().await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated database");
}

#[tokio::test]
async fn batch_round_trip_reuses_one_subject_kek_and_rejects_mixed_groups_db() {
    // Pins: one grouped batch preserves order and one KEK, while mixed subjects
    // and tenants fail before they can be coalesced into a scoped transaction.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = connect(&database_url, 8).await;
    let provider = PostgresKmsProvider::new(pool.clone(), keyring("primary", &[("primary", 5)]));
    let tenant = Uuid::now_v7();
    let subject = Uuid::now_v7();
    let contexts = (0..4)
        .map(|index| {
            EncryptionContext::new(tenant, subject, format!("record-{index}"), "restricted")
        })
        .collect::<Vec<_>>();
    let generated = provider
        .generate_data_keys(&contexts)
        .await
        .expect("generate grouped batch");
    assert_eq!(generated.len(), contexts.len());
    assert!(
        generated
            .iter()
            .all(|data_key| data_key.handle == generated[0].handle)
    );
    assert_eq!(count_kek_rows(&pool, tenant, subject).await, 1);

    let requests = generated
        .iter()
        .zip(&contexts)
        .map(|(data_key, context)| {
            DataKeyDecryptRequest::new(
                data_key.wrapped.clone(),
                data_key.handle.clone(),
                context.clone(),
            )
        })
        .collect::<Vec<_>>();
    let decrypted = provider
        .decrypt_data_keys(&requests)
        .await
        .expect("decrypt grouped batch");
    assert_eq!(decrypted.len(), generated.len());
    for (opened, expected) in decrypted.iter().zip(&generated) {
        assert_eq!(opened.expose(), expected.plaintext.expose());
    }

    let other_subject = Uuid::now_v7();
    let mixed_subjects = [
        contexts[0].clone(),
        EncryptionContext::new(tenant, other_subject, "other", "restricted"),
    ];
    let error = provider
        .generate_data_keys(&mixed_subjects)
        .await
        .expect_err("mixed subjects must fail");
    assert!(matches!(error, CryptoError::InvalidBatch(_)));
    assert_eq!(count_kek_rows(&pool, tenant, other_subject).await, 0);

    let mixed_tenants = [
        contexts[0].clone(),
        EncryptionContext::new(Uuid::now_v7(), subject, "other", "restricted"),
    ];
    assert!(matches!(
        provider.generate_data_keys(&mixed_tenants).await,
        Err(CryptoError::InvalidBatch(_))
    ));

    pool.close().await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated database");
}

#[tokio::test]
async fn crypto_shred_is_durable_and_subject_isolated_db() {
    // Pins: a subject tombstone survives provider restart and leaves another
    // subject in the same tenant decryptable.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = connect(&database_url, 8).await;
    let tenant = Uuid::now_v7();
    let ctx_a = EncryptionContext::new(tenant, Uuid::now_v7(), "record-a", "restricted");
    let ctx_b = EncryptionContext::new(tenant, Uuid::now_v7(), "record-b", "restricted");
    let provider = PostgresKmsProvider::new(pool.clone(), keyring("primary", &[("primary", 6)]));
    let a = provider
        .generate_data_key(&ctx_a)
        .await
        .expect("generate A");
    let b = provider
        .generate_data_key(&ctx_b)
        .await
        .expect("generate B");
    provider
        .destroy_subject_key(tenant, ctx_a.subject_id)
        .await
        .expect("shred A");
    provider
        .destroy_subject_key(tenant, ctx_a.subject_id)
        .await
        .expect("shred A idempotently");
    drop(provider);

    let restarted = PostgresKmsProvider::new(pool.clone(), keyring("primary", &[("primary", 6)]));
    assert!(matches!(
        restarted
            .decrypt_data_key(&a.wrapped, &a.handle, &ctx_a)
            .await,
        Err(CryptoError::CryptoShredded(_))
    ));
    restarted
        .decrypt_data_key(&b.wrapped, &b.handle, &ctx_b)
        .await
        .expect("B remains decryptable");

    pool.close().await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated database");
}
