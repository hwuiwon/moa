//! DB coverage for provider source ACL capture during ingestion.

use super::*;

use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_knowledge::acl_key::{KmsSourceAclKeyOwner, SourceAclKey, SourceAclKeyOwner};
use moa_knowledge::domain::{
    CanonicalSourcePrincipal, ProviderAclEntry, ProviderRecordAcl, SourceAclEntryKind,
    SourceAclState, SourcePrincipalKind,
};

/// The fixture ACL key. One fixed version and material, so a fingerprint
/// computed in the test matches the one ingestion persisted.
fn acl_key() -> Arc<SourceAclKey> {
    Arc::new(SourceAclKey::new(1, vec![0x5A; 32]))
}

fn principal(subject: &str, kind: SourcePrincipalKind) -> CanonicalSourcePrincipal {
    CanonicalSourcePrincipal::new("nango:google-drive", kind, subject).expect("normalizes")
}

/// Builds one already-keyed ACL entry, exactly as an adapter would emit it.
fn grant(
    entry_kind: SourceAclEntryKind,
    subject: &str,
    kind: SourcePrincipalKind,
) -> ProviderAclEntry {
    ProviderAclEntry {
        entry_kind,
        principal_kind: kind,
        principal: acl_key().fingerprint(&principal(subject, kind)),
    }
}

/// Builds a pipeline for a permission-bearing connection.
async fn source_acl_pipeline(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
) -> (
    Arc<PostgresKnowledgeRepository>,
    Arc<CountingEmbedder>,
    KnowledgeIngestionPipeline<
        PostgresKnowledgeRepository,
        ParagraphParser,
        CountingEmbedder,
        FakeGraphWriter,
    >,
) {
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        RlsContext::tenant(tenant_id),
    ));
    let embedder = Arc::new(CountingEmbedder::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(ParagraphParser),
        embedder.clone(),
        Arc::new(FakeGraphWriter::default()),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "nango".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );
    let mut connection = drive_connection(connection_uid, tenant_id);
    connection.provider = "nango".to_string();
    repository
        .upsert_connection(connection)
        .await
        .expect("upsert permission-bearing connection");
    (repository, embedder, pipeline)
}

/// One provider record carrying inline text plus a native ACL.
fn acl_record(
    source_id: &str,
    change_token: &str,
    text: &str,
    revision: &str,
    complete: bool,
    grants: Vec<ProviderAclEntry>,
) -> ProviderRecord {
    ProviderRecord {
        source_id: source_id.to_string(),
        object_type: "drive_file".to_string(),
        title: Some(source_id.to_string()),
        source_uri: None,
        change_token: Some(change_token.to_string()),
        deleted: false,
        source_updated_at: Some(moa_test_support::fixtures::pg_now()),
        metadata: json!({}),
        payload: json!({ "content": text }),
        acl: ProviderRecordAcl {
            provider_revision: revision.to_string(),
            complete,
            entries: grants,
        },
    }
}

#[tokio::test]
async fn first_acl_key_mint_is_tenant_scoped_under_the_app_role_db_memory() {
    // Pins: the first-key INSERT and the concurrent-winner SELECT both run
    // under the requested tenant's RLS scope. A nil scope makes this fail as
    // `moa_app`, even though later cached reads would appear healthy.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let kms: Arc<dyn KeyManagementProvider> = Arc::new(LocalKmsProvider::new());
    let owner_a = KmsSourceAclKeyOwner::new_for_app_role(pool.clone(), Arc::clone(&kms));
    let owner_b = KmsSourceAclKeyOwner::new_for_app_role(pool.clone(), Arc::clone(&kms));

    let (key_a, key_b) = tokio::join!(
        owner_a.current_key(tenant_id),
        owner_b.current_key(tenant_id)
    );
    let key_a = key_a.expect("first app-role mint");
    let key_b = key_b.expect("concurrent app-role lookup");
    assert_eq!(key_a.key_version(), 1);
    assert_eq!(key_b.key_version(), 1);

    let stored = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.knowledge_source_acl_keys WHERE tenant_id = $1",
    )
    .bind(tenant_id.0)
    .fetch_one(&pool)
    .await
    .expect("count persisted keys");
    assert_eq!(stored, 1, "the concurrent mint converges on one tenant key");
}

#[tokio::test]
async fn acl_only_change_flips_visibility_without_reparsing_or_reembedding_db_memory() {
    // Pins the whole point of capturing the ACL ahead of the content fences: a
    // sync pass whose content is byte-identical but whose permissions changed
    // must (a) store a NEW snapshot at the new provider revision, (b) leave the
    // object `current` and pointing at it, and (c) do so without producing a
    // single new embedding — because nothing about the document's text changed.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, embedder, pipeline) =
        source_acl_pipeline(&pool, tenant_id, connection_uid).await;

    const TEXT: &str = "Executive bonuses are approved quarterly by the board.";
    let key = acl_key();
    let alice = key.fingerprint(&principal("alice@example.com", SourcePrincipalKind::User));
    let bob = key.fingerprint(&principal("bob@example.com", SourcePrincipalKind::User));

    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "memo",
                    "etag-1",
                    TEXT,
                    "acl-rev-1",
                    true,
                    vec![grant(
                        SourceAclEntryKind::Allow,
                        "alice@example.com",
                        SourcePrincipalKind::User,
                    )],
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("first ingestion");

    let object = repository
        .get_object_by_source(connection_uid, "memo")
        .await
        .expect("read object")
        .expect("object exists");
    let first_acl = repository
        .object_acl(object.object_uid)
        .await
        .expect("read acl")
        .expect("acl exists");
    assert_eq!(first_acl.state, SourceAclState::Current);
    assert_eq!(first_acl.revision.as_deref(), Some("acl-rev-1"));
    let first_snapshot = first_acl
        .current_snapshot_uid
        .expect("a current object names its snapshot");
    assert_eq!(
        repository
            .snapshot_entries(first_snapshot)
            .await
            .expect("read entries")
            .into_iter()
            .map(|entry| (entry.entry_kind, entry.principal))
            .collect::<Vec<_>>(),
        vec![(SourceAclEntryKind::Allow, alice.clone())]
    );
    let embeddings_after_first = embedder.embedded_count();
    assert!(embeddings_after_first > 0, "first pass must embed the text");

    // Same change token, same bytes — only the sharing changed.
    let acl_only_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            acl_only_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "memo",
                    "etag-1",
                    TEXT,
                    "acl-rev-2",
                    true,
                    vec![
                        grant(
                            SourceAclEntryKind::Allow,
                            "bob@example.com",
                            SourcePrincipalKind::User,
                        ),
                        grant(
                            SourceAclEntryKind::Deny,
                            "alice@example.com",
                            SourcePrincipalKind::User,
                        ),
                    ],
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("acl-only ingestion");

    let second_acl = repository
        .object_acl(object.object_uid)
        .await
        .expect("read acl")
        .expect("acl exists");
    assert_eq!(second_acl.state, SourceAclState::Current);
    assert_eq!(second_acl.revision.as_deref(), Some("acl-rev-2"));
    let second_snapshot = second_acl
        .current_snapshot_uid
        .expect("a current object names its snapshot");
    assert_ne!(
        second_snapshot, first_snapshot,
        "a new provider revision must mint a new immutable snapshot"
    );
    let mut entries = repository
        .snapshot_entries(second_snapshot)
        .await
        .expect("read entries")
        .into_iter()
        .map(|entry| (entry.entry_kind, entry.principal))
        .collect::<Vec<_>>();
    entries.sort();
    let mut expected = vec![
        (SourceAclEntryKind::Allow, bob),
        (SourceAclEntryKind::Deny, alice),
    ];
    expected.sort();
    assert_eq!(entries, expected);

    assert_eq!(
        embedder.embedded_count(),
        embeddings_after_first,
        "an ACL-only change must not re-embed a single chunk"
    );

    // The earlier snapshot survives as immutable evidence of what was true.
    assert!(
        !repository
            .snapshot_entries(first_snapshot)
            .await
            .expect("read entries")
            .is_empty(),
        "the superseded snapshot is retained, not rewritten"
    );
}

#[tokio::test]
async fn unchanged_anyone_acl_is_idempotent_but_changed_entries_get_a_new_snapshot_db_memory() {
    // Pins: a complete Anyone ACL writes exactly one connection-scoped
    // tenant-wide binding. Replaying the same capture changes no ACL epoch,
    // while changing canonical entries under the same provider revision gets a
    // distinct immutable snapshot and moves the object's pointer.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, _embedder, pipeline) =
        source_acl_pipeline(&pool, tenant_id, connection_uid).await;
    let anyone = grant(SourceAclEntryKind::Allow, "", SourcePrincipalKind::Anyone);

    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "shared",
                    "etag-1",
                    "Shared provider document.",
                    "acl-rev-stable",
                    true,
                    vec![anyone.clone()],
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("first Anyone capture");

    let object = repository
        .get_object_by_source(connection_uid, "shared")
        .await
        .expect("read object")
        .expect("object exists");
    let first_snapshot = repository
        .object_acl(object.object_uid)
        .await
        .expect("read ACL")
        .expect("ACL exists")
        .current_snapshot_uid
        .expect("complete ACL is current");
    let epoch_after_first = moa_db::current_source_acl_epoch(&pool, tenant_id, true)
        .await
        .expect("read ACL epoch");

    let binding = sqlx::query_as::<_, (Uuid, Uuid, String, Vec<u8>)>(
        r#"
        SELECT contact_id, connection_id, principal_kind, principal_fingerprint
        FROM moa.knowledge_source_principal_bindings
        WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id.0)
    .fetch_one(&pool)
    .await
    .expect("read generated Anyone binding");
    assert_eq!(binding.0, moa_db::TENANT_WIDE_PRINCIPAL_HOLDER);
    assert_eq!(binding.1, connection_uid);
    assert_eq!(binding.2, "anyone");
    assert_eq!(binding.3, anyone.principal.as_bytes());

    let replay_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            replay_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "shared",
                    "etag-1",
                    "Shared provider document.",
                    "acl-rev-stable",
                    true,
                    vec![anyone],
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("identical replay");
    assert_eq!(
        moa_db::current_source_acl_epoch(&pool, tenant_id, true)
            .await
            .expect("read replay epoch"),
        epoch_after_first,
        "an unchanged snapshot and binding must not invalidate ACL caches"
    );

    let changed_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            changed_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "shared",
                    "etag-1",
                    "Shared provider document.",
                    "acl-rev-stable",
                    true,
                    vec![grant(
                        SourceAclEntryKind::Allow,
                        "alice@example.com",
                        SourcePrincipalKind::User,
                    )],
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("same revision with changed entries");
    let changed_snapshot = repository
        .object_acl(object.object_uid)
        .await
        .expect("read changed ACL")
        .expect("ACL exists")
        .current_snapshot_uid
        .expect("complete ACL is current");
    assert_ne!(
        changed_snapshot, first_snapshot,
        "canonical entries are part of snapshot identity"
    );
}

#[tokio::test]
async fn incomplete_provider_acl_hides_the_object_before_the_typed_error_db_memory() {
    // Pins the ordering that matters when a provider cannot enumerate
    // permissions: the object is moved to `incomplete` — invisible — and only
    // THEN does the typed error propagate. If the error came first, a document
    // whose sharing MOA can no longer read would stay retrievable for as long as
    // the sync kept failing.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, _embedder, pipeline) =
        source_acl_pipeline(&pool, tenant_id, connection_uid).await;

    let run = create_run(&repository, tenant_id, connection_uid).await;
    let error = pipeline
        .ingest_record_page(
            run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "partial",
                    "etag-1",
                    "Salary bands for the engineering ladder.",
                    "acl-rev-1",
                    false,
                    vec![grant(
                        SourceAclEntryKind::Allow,
                        "alice@example.com",
                        SourcePrincipalKind::User,
                    )],
                )],
                next_cursor: None,
            },
        )
        .await
        .expect_err("an incomplete provider ACL must fail the record");
    assert!(
        error.to_string().contains("incomplete ACL"),
        "unexpected error: {error}"
    );

    let object = repository
        .get_object_by_source(connection_uid, "partial")
        .await
        .expect("read object")
        .expect("the object row exists so its ACL could be recorded against it");
    let acl = repository
        .object_acl(object.object_uid)
        .await
        .expect("read acl")
        .expect("acl exists");
    assert_eq!(
        acl.state,
        SourceAclState::Incomplete,
        "an object whose permissions could not be enumerated must be hidden"
    );
    assert_eq!(
        acl.current_snapshot_uid, None,
        "an incomplete capture must never become the current snapshot"
    );
    assert!(
        !acl.admits(),
        "the recorded position must refuse every caller"
    );

    let recovery_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            recovery_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "partial",
                    "etag-1",
                    "Salary bands for the engineering ladder.",
                    "acl-rev-1",
                    true,
                    vec![grant(
                        SourceAclEntryKind::Allow,
                        "alice@example.com",
                        SourcePrincipalKind::User,
                    )],
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("a later complete listing with the same revision recovers");
    assert_eq!(
        repository
            .object_acl(object.object_uid)
            .await
            .expect("read recovered ACL")
            .expect("ACL exists")
            .state,
        SourceAclState::Current,
        "the incomplete snapshot identity must not block a complete replacement"
    );
}
