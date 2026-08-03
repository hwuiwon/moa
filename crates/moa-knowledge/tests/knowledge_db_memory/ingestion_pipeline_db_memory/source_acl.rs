//! DB coverage for provider source ACL capture during ingestion.

use super::*;

use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_knowledge::acl_key::{KmsSourceAclKeyOwner, SourceAclKey, SourceAclKeyOwner};
use moa_knowledge::domain::{
    CanonicalSourcePrincipal, MAX_SOURCE_ACL_ENTRIES, ProviderAclEntry, ProviderRecordAcl,
    SourceAclEntryKind, SourceAclState, SourcePrincipalBinding, SourcePrincipalKind,
};
use moa_knowledge::repository::acl::KnowledgeAclRepository;

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

fn indexed_grants(count: usize) -> Vec<ProviderAclEntry> {
    (0..count)
        .map(|index| {
            grant(
                SourceAclEntryKind::Allow,
                &format!("user-{index}@example.com"),
                SourcePrincipalKind::User,
            )
        })
        .collect()
}

async fn chunk_is_admitted(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    chunk_uid: Uuid,
    acl: &moa_core::types::memory::SourceAclContext,
) -> bool {
    let mut conn = moa_db::ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true)
        .await
        .expect("open app-role ACL admission transaction");
    let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT EXISTS (SELECT 1 FROM moa.knowledge_chunks AS chunk WHERE chunk.chunk_uid = ",
    );
    builder.push_bind(chunk_uid).push(" AND ");
    moa_db::push_source_acl_predicate(&mut builder, "chunk.chunk_uid", acl);
    builder.push(")");
    let admitted = builder
        .build_query_scalar::<bool>()
        .fetch_one(conn.as_mut())
        .await
        .expect("evaluate source ACL admission");
    conn.commit()
        .await
        .expect("commit ACL admission transaction");
    admitted
}

/// Builds a pipeline for a permission-bearing connection.
async fn source_acl_pipeline(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
) -> (
    Arc<PostgresKnowledgeRepository>,
    Arc<CountingEmbedder>,
    KnowledgeIngestionPipeline,
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
    connection.provider = moa_knowledge::domain::LinkedProviderKind::Nango;
    insert_managed_connector_parent(
        pool,
        tenant_id,
        connection_uid,
        moa_knowledge::domain::LinkedProviderKind::Nango,
    )
    .await;
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
        materialization: ProviderRecordMaterialization::InlineText {
            text: text.to_string(),
            mime_type: None,
        },
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
async fn contact_offboarding_with_multiple_principals_bumps_acl_epoch_once_db_memory() {
    // Pins: contact offboarding removes all provider principals in one DELETE,
    // so cache invalidation takes one tenant-epoch lock regardless of how many
    // bindings the contact accumulated.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = Uuid::now_v7();
    let connection_uid = Uuid::now_v7();
    let (repository, _embedder, _pipeline) =
        source_acl_pipeline(&pool, tenant_id, connection_uid).await;

    for subject in [
        "alice@example.com",
        "alice@contractor.example",
        "alice@subsidiary.example",
    ] {
        repository
            .upsert_principal_binding(SourcePrincipalBinding {
                binding_uid: Uuid::now_v7(),
                tenant_id,
                contact_id,
                connection_uid: Some(connection_uid),
                principal_kind: SourcePrincipalKind::User,
                principal: acl_key().fingerprint(&principal(subject, SourcePrincipalKind::User)),
                verified_at: moa_test_support::fixtures::pg_now(),
            })
            .await
            .expect("bind one verified provider principal");
    }

    let epoch_before_offboarding = moa_db::current_source_acl_epoch(&pool, tenant_id, true)
        .await
        .expect("read ACL epoch before offboarding");
    assert_eq!(
        repository
            .revoke_contact_principals(contact_id)
            .await
            .expect("offboard contact principals"),
        3,
        "offboarding removes every binding in one production repository call"
    );
    assert_eq!(
        moa_db::current_source_acl_epoch(&pool, tenant_id, true)
            .await
            .expect("read ACL epoch after offboarding"),
        epoch_before_offboarding + 1,
        "one bulk DELETE must bump the affected tenant exactly once"
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

#[tokio::test]
async fn exact_limit_acl_persists_every_canonical_entry_without_cross_tenant_visibility_db_memory()
{
    // Pins: the 4,096-entry boundary uses the real repository write path and
    // persists exactly that canonical set, while an app-role repository scoped
    // to another tenant cannot observe the object or its snapshot entries.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, _embedder, pipeline) =
        source_acl_pipeline(&pool, tenant_id, connection_uid).await;
    let grants = indexed_grants(MAX_SOURCE_ACL_ENTRIES);
    let admitted_principal = grants
        .first()
        .expect("the exact-limit fixture has an entry")
        .principal
        .clone();

    let run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "at-limit",
                    "etag-1",
                    "A document with a large but bounded native ACL.",
                    "acl-rev-limit",
                    true,
                    grants,
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("the exact ACL entry limit persists");

    let object = repository
        .get_object_by_source(connection_uid, "at-limit")
        .await
        .expect("read object")
        .expect("object exists");
    let snapshot_uid = repository
        .object_acl(object.object_uid)
        .await
        .expect("read ACL")
        .expect("ACL exists")
        .current_snapshot_uid
        .expect("the exact-limit snapshot is current");
    assert_eq!(
        repository
            .snapshot_entries(snapshot_uid)
            .await
            .expect("read exact-limit entries")
            .len(),
        MAX_SOURCE_ACL_ENTRIES
    );

    let chunk_uid = repository
        .active_chunks_for_object(object.object_uid)
        .await
        .expect("read active chunks")
        .into_iter()
        .next()
        .expect("ingestion produced a chunk")
        .chunk_uid;
    let acl_context = moa_core::types::memory::SourceAclContext::new([admitted_principal], 0);
    assert!(
        chunk_is_admitted(&pool, tenant_id, chunk_uid, &acl_context).await,
        "the source principal is admitted inside the owning tenant"
    );
    assert!(
        !chunk_is_admitted(&pool, other_tenant_id, chunk_uid, &acl_context).await,
        "the same opaque principal cannot admit another tenant's chunk"
    );

    let other_repository =
        PostgresKnowledgeRepository::scoped_for_app_role(pool, RlsContext::tenant(other_tenant_id));
    assert_eq!(
        other_repository
            .get_object(object.object_uid)
            .await
            .expect("cross-tenant object read is denied"),
        None
    );
    assert_eq!(
        other_repository
            .snapshot_entries(snapshot_uid)
            .await
            .expect("cross-tenant snapshot read is denied"),
        Vec::new()
    );
}

#[tokio::test]
async fn oversized_acl_is_empty_incomplete_and_hides_old_grant_db_memory() {
    // Pins: an oversized canonical provider snapshot replaces a previously
    // visible ACL atomically with an incomplete position, persists zero partial
    // entries, and retains the old immutable snapshot only as non-current
    // evidence.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let (repository, _embedder, pipeline) =
        source_acl_pipeline(&pool, tenant_id, connection_uid).await;
    let alice_grant = grant(
        SourceAclEntryKind::Allow,
        "alice@example.com",
        SourcePrincipalKind::User,
    );
    let alice_acl =
        moa_core::types::memory::SourceAclContext::new([alice_grant.principal.clone()], 0);

    let visible_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            visible_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "oversized-replacement",
                    "etag-1",
                    "A source document whose ACL later exceeds the bound.",
                    "acl-rev-visible",
                    true,
                    vec![alice_grant],
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("initial bounded ACL is visible");
    let object = repository
        .get_object_by_source(connection_uid, "oversized-replacement")
        .await
        .expect("read object")
        .expect("object exists");
    let old_snapshot_uid = repository
        .object_acl(object.object_uid)
        .await
        .expect("read visible ACL")
        .expect("ACL exists")
        .current_snapshot_uid
        .expect("bounded ACL is current");
    let chunk_uid = repository
        .active_chunks_for_object(object.object_uid)
        .await
        .expect("read active chunks")
        .into_iter()
        .next()
        .expect("ingestion produced a chunk")
        .chunk_uid;
    assert!(
        chunk_is_admitted(&pool, tenant_id, chunk_uid, &alice_acl).await,
        "the original bounded allow is visible before replacement"
    );

    let oversized_run = create_run(&repository, tenant_id, connection_uid).await;
    let error = pipeline
        .ingest_record_page(
            oversized_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![acl_record(
                    "oversized-replacement",
                    "etag-1",
                    "A source document whose ACL later exceeds the bound.",
                    "acl-rev-oversized",
                    true,
                    indexed_grants(MAX_SOURCE_ACL_ENTRIES + 1),
                )],
                next_cursor: None,
            },
        )
        .await
        .expect_err("an oversized canonical ACL must fail closed");
    assert!(
        error.to_string().contains("incomplete ACL"),
        "unexpected error: {error}"
    );

    let hidden_acl = repository
        .object_acl(object.object_uid)
        .await
        .expect("read hidden ACL")
        .expect("ACL exists");
    assert_eq!(hidden_acl.state, SourceAclState::Incomplete);
    assert_eq!(hidden_acl.revision, None);
    assert_eq!(hidden_acl.current_snapshot_uid, None);
    assert!(!hidden_acl.admits());
    assert!(
        !chunk_is_admitted(&pool, tenant_id, chunk_uid, &alice_acl).await,
        "an oversized replacement must revoke admission before returning its error"
    );

    let (oversized_snapshot_uid, complete, entry_count) = sqlx::query_as::<_, (Uuid, bool, i32)>(
        r#"
            SELECT snapshot_uid, complete, entry_count
            FROM moa.knowledge_source_acl_snapshots
            WHERE tenant_id = $1
              AND object_id = $2
              AND provider_revision = 'acl-rev-oversized'
            "#,
    )
    .bind(tenant_id.0)
    .bind(object.object_uid)
    .fetch_one(&pool)
    .await
    .expect("read oversized snapshot evidence");
    assert!(!complete);
    assert_eq!(entry_count, 0);
    assert_eq!(
        repository
            .snapshot_entries(oversized_snapshot_uid)
            .await
            .expect("read oversized snapshot entries"),
        Vec::new(),
        "an oversized capture must persist no partial permission entries"
    );
    assert_eq!(
        repository
            .snapshot_entries(old_snapshot_uid)
            .await
            .expect("read old immutable entries")
            .len(),
        1,
        "replacement must not mutate the prior evidence snapshot"
    );
}
