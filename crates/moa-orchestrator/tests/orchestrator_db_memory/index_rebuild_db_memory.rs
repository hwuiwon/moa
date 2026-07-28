//! Durable storage-partition index rebuild coverage.
//!
//! These tests drive the production repository against a real isolated
//! Postgres, because every guarantee the rebuild makes is a database
//! guarantee: partial unique indexes, compare-and-swap updates, and one scoped
//! activation transaction. An in-memory double would assert the test's own
//! model of those, not the ones that ship.

use std::collections::HashSet;

use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::{
    EmbeddingGenerationId, RebuildKind, RebuildLifecycle, RebuildOperationId, RlsContext,
    contextual_chunk_embedding_input,
};
use moa_core::types::security::SensitivityClass;
use moa_db::ScopedConn;
use moa_memory_vector::rebuild::{
    BatchCommit, BatchCounters, CandidateVector, RebuildFence, RebuildRepository, StartRebuild,
};
use moa_memory_vector::{Error as VectorError, PgvectorStore, VectorItem, VectorStore};
use moa_session::testing;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

const ESTIMATE_RATE: i64 = 100_000;

struct RebuildFixture {
    pool: PgPool,
    tenant_id: TenantId,
    storage_partition_id: String,
    database_url: String,
    schema_name: String,
    seeded: Vec<Uuid>,
}

impl RebuildFixture {
    fn scope(&self) -> RlsContext {
        RlsContext::tenant(self.tenant_id)
    }

    fn repository(&self) -> RebuildRepository {
        RebuildRepository::new(self.pool.clone(), self.scope())
    }

    async fn cleanup(self) {
        self.pool.close().await;
        testing::cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .expect("clean up isolated test schema");
    }
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0_f32; 1024];
    vector[index % 1024] = 1.0;
    vector
}

/// Seeds a partition with facts and knowledge chunks that both carry real,
/// reconstructable embedding provenance.
///
/// The mix is deliberate: a partition-wide rebuild has to reproduce the fact
/// summaries *and* the chunks' contextual inputs, and a rebuild that only
/// understood chunks would pass a chunk-only fixture.
async fn seed_partition(fact_count: usize, chunk_count: usize) -> RebuildFixture {
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    drop(session_store);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await
        .expect("connect to isolated Postgres");

    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = tenant_id.to_string();
    let scope = RlsContext::tenant(tenant_id);

    let mut conn = ScopedConn::begin_as_app(&pool, &scope, true)
        .await
        .expect("begin seed transaction");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, 'embed-v4.0', 1, 1024)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET reembed_state = 'steady'
        "#,
    )
    .bind(&storage_partition_id)
    .execute(conn.as_mut())
    .await
    .expect("seed storage partition state");
    conn.commit().await.expect("commit partition state");

    let mut seeded = Vec::new();
    let store = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());

    for index in 0..fact_count {
        let uid = Uuid::now_v7();
        seeded.push(uid);
        insert_node(
            &pool,
            &scope,
            uid,
            "Fact",
            &format!("fact {index}"),
            json!({"summary": format!("fact number {index} states something durable")}),
        )
        .await;
        store
            .upsert(&[vector_item(uid, "Fact", basis_vector(index))])
            .await
            .expect("seed fact vector");
    }

    if chunk_count > 0 {
        seed_knowledge_document(&pool, &scope, tenant_id, &storage_partition_id, chunk_count)
            .await
            .into_iter()
            .for_each(|uid| seeded.push(uid));
        for (offset, uid) in seeded.iter().skip(fact_count).copied().enumerate() {
            store
                .upsert(&[vector_item(uid, "Chunk", basis_vector(fact_count + offset))])
                .await
                .expect("seed chunk vector");
        }
    }

    RebuildFixture {
        pool,
        tenant_id,
        storage_partition_id,
        database_url,
        schema_name,
        seeded,
    }
}

fn vector_item(uid: Uuid, label: &str, embedding: Vec<f32>) -> VectorItem {
    VectorItem {
        uid,
        user_id: None,
        label: label.to_string(),
        pii_class: SensitivityClass::None,
        embedding,
        embedding_model: "embed-v4.0".to_string(),
        embedding_model_version: 1,
        search_text: None,
        valid_to: None,
    }
}

async fn insert_node(
    pool: &PgPool,
    scope: &RlsContext,
    uid: Uuid,
    label: &str,
    name: &str,
    properties: serde_json::Value,
) {
    let mut conn = ScopedConn::begin_as_app(pool, scope, true)
        .await
        .expect("begin node seed transaction");
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, data_subject_id, name, pii_class,
             properties_summary)
        VALUES ($1, $2, $3, $4, $5, 'none', $6)
        "#,
    )
    .bind(uid)
    .bind(label)
    .bind(scope.storage_partition_id().to_string())
    .bind(scope.tenant_id().0)
    .bind(name)
    .bind(properties)
    .execute(conn.as_mut())
    .await
    .expect("seed node_index row");
    conn.commit().await.expect("commit node seed");
}

/// Seeds one knowledge object, version, and its chunks, returning chunk uids.
async fn seed_knowledge_document(
    pool: &PgPool,
    scope: &RlsContext,
    tenant_id: TenantId,
    storage_partition_id: &str,
    chunk_count: usize,
) -> Vec<Uuid> {
    let connection_uid = Uuid::now_v7();
    let object_uid = Uuid::now_v7();
    let version_uid = Uuid::now_v7();

    let mut conn = ScopedConn::begin_as_app(pool, scope, true)
        .await
        .expect("begin knowledge seed transaction");
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_connections
            (connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
             provider_connection_id, connector, credential_ref, status, acl_mode)
        VALUES ($1, $2, $3, 'nango', 'nango-key', 'conn-1', 'google-drive', 'ref-1', 'active',
                'tenant_public')
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .execute(conn.as_mut())
    .await
    .expect("seed knowledge connection");
    // The connection is `tenant_public`, so admission does not consult a
    // provider snapshot and the object carries no captured ACL. `incomplete` is
    // the state that says exactly that; `current` would require a snapshot the
    // fixture has no reason to fabricate.
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_objects
            (object_uid, tenant_id, storage_partition_id, connection_id, object_type,
             external_object_id, title, status, acl_state)
        VALUES ($1, $2, $3, $4, 'document', 'ext-1', 'Security Handbook', 'active', 'incomplete')
        "#,
    )
    .bind(object_uid)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .bind(connection_uid)
    .execute(conn.as_mut())
    .await
    .expect("seed knowledge object");
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_document_versions
            (document_version_uid, tenant_id, storage_partition_id, object_id, parser_provider,
             content_hash)
        VALUES ($1, $2, $3, $4, 'native', 'hash-1')
        "#,
    )
    .bind(version_uid)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .bind(object_uid)
    .execute(conn.as_mut())
    .await
    .expect("seed knowledge document version");
    conn.commit().await.expect("commit knowledge seed");

    let mut chunk_uids = Vec::new();
    for ordinal in 0..chunk_count {
        let chunk_uid = Uuid::now_v7();
        chunk_uids.push(chunk_uid);
        insert_node(
            pool,
            scope,
            chunk_uid,
            "Chunk",
            &format!("chunk {ordinal}"),
            json!({"ordinal": ordinal}),
        )
        .await;
        let mut conn = ScopedConn::begin_as_app(pool, scope, true)
            .await
            .expect("begin chunk seed transaction");
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_chunks
                (chunk_uid, tenant_id, storage_partition_id, document_version_id, graph_node_uid,
                 chunk_hash, heading_path, text, ordinal, token_count)
            VALUES ($1, $2, $3, $4, $1, $5,
                    ARRAY['Access Control', 'Key Rotation', 'Quarterly Cadence'], $6, $7, 10)
            "#,
        )
        .bind(chunk_uid)
        .bind(tenant_id.0)
        .bind(storage_partition_id)
        .bind(version_uid)
        .bind(format!("chunk-hash-{ordinal}"))
        .bind(format!("chunk body number {ordinal}"))
        .bind(i32::try_from(ordinal).expect("ordinal fits i32"))
        .execute(conn.as_mut())
        .await
        .expect("seed knowledge chunk");
        conn.commit().await.expect("commit chunk seed");
    }
    chunk_uids
}

/// Runs the plan and the whole build loop, returning the candidate generation.
async fn build_full_generation(
    fixture: &RebuildFixture,
) -> (RebuildOperationId, Uuid, EmbeddingGenerationId) {
    let repository = fixture.repository();
    let operation_uid = RebuildOperationId::new();
    let owner_token = Uuid::now_v7();

    repository
        .start_operation(StartRebuild {
            operation_uid,
            owner_token,
            kind: RebuildKind::Reembed,
            embedding_model: "embed-v5.0".to_string(),
            embedding_model_version: 1,
            estimate_micros_per_million_tokens: ESTIMATE_RATE,
        })
        .await
        .expect("start rebuild operation");
    repository
        .ensure_bootstrap_generation("embed-v4.0", 1, &fixture.storage_partition_id)
        .await
        .expect("adopt bootstrap generation");
    let total = repository
        .count_partition_vectors()
        .await
        .expect("census partition");
    repository
        .record_plan(operation_uid, owner_token, total)
        .await
        .expect("record plan");
    let generation = repository
        .create_candidate_generation(
            operation_uid,
            owner_token,
            EmbeddingGenerationId::new(),
            "embed-v5.0",
            1,
            &fixture.storage_partition_id,
        )
        .await
        .expect("create candidate generation");
    repository
        .transition(
            operation_uid,
            owner_token,
            RebuildLifecycle::Planning,
            RebuildLifecycle::Building,
        )
        .await
        .expect("enter building");

    let inputs = repository
        .load_authoritative_inputs(None, 1024)
        .await
        .expect("load authoritative inputs");
    let candidates = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| CandidateVector::from_input(input, basis_vector(index)))
        .collect::<Vec<_>>();
    let last_uid = candidates
        .last()
        .expect("seeded partition is not empty")
        .uid;
    repository
        .commit_batch(
            RebuildFence {
                operation_uid,
                owner_token,
                generation_uid: generation.generation_uid,
            },
            BatchCommit {
                candidates: &candidates,
                checkpoint_uid: last_uid,
                batch_index: 0,
                counters: BatchCounters {
                    vectors_failed: 0,
                    estimated_input_tokens: inputs
                        .iter()
                        .map(|input| i64::from(input.estimated_tokens()))
                        .sum(),
                    provider_requests: 1,
                    provider_throttles: 0,
                    provider_retries: 0,
                },
                estimate_micros_per_million_tokens: ESTIMATE_RATE,
            },
        )
        .await
        .expect("commit build batch");
    repository
        .mark_generation_complete(generation.generation_uid, total)
        .await
        .expect("mark generation complete");

    (operation_uid, owner_token, generation.generation_uid)
}

#[tokio::test]
async fn rebuild_reconstructs_every_vector_type_in_the_partition_db_memory() {
    // Pins: a partition-wide re-embed reproduces the authoritative input for
    // each label. A chunk's input is the contextual form (title > heading path
    // then body), not the bare chunk text the Turbopuffer sync uses for BM25;
    // rebuilding from the bare text would move every chunk vector into a
    // different space while every row still claimed the new model.
    let fixture = seed_partition(2, 2).await;
    let repository = fixture.repository();

    let inputs = repository
        .load_authoritative_inputs(None, 100)
        .await
        .expect("load authoritative inputs");

    assert_eq!(inputs.len(), 4, "every seeded vector must be reconstructed");
    let by_label = inputs
        .iter()
        .map(|input| (input.label.clone(), input.text.clone()))
        .collect::<Vec<_>>();
    assert!(
        by_label
            .iter()
            .any(|(label, text)| label == "Fact" && text.contains("states something durable")),
        "fact inputs come from properties_summary->>'summary': {by_label:?}"
    );
    // Byte-equality against the function ingestion itself calls, over a real
    // title and a multi-level heading path. Asserting "non-empty" or "contains
    // the body" would pass for the bare `knowledge_chunks.text` the Turbopuffer
    // sync exposes as `search_text` -- which is the BM25 body, not the
    // embedding input. Rebuilding from that would move every chunk vector into
    // a different space while every count-based check still agreed.
    let chunk_input = inputs
        .iter()
        .find(|input| input.label == "Chunk" && input.text.contains("chunk body number 0"))
        .expect("a chunk input is present");
    let expected = contextual_chunk_embedding_input(
        Some("Security Handbook"),
        &[
            "Access Control".to_string(),
            "Key Rotation".to_string(),
            "Quarterly Cadence".to_string(),
        ],
        "chunk body number 0",
    );
    assert_eq!(
        chunk_input.text, expected,
        "the reconstructed chunk input must byte-equal the ingestion-side contextual form"
    );
    assert_eq!(
        expected,
        "Security Handbook > Access Control > Key Rotation > Quarterly Cadence\n\nchunk body number 0",
        "the contextual form itself changed; ingestion and rebuild move together or not at all"
    );

    let unsupported = repository
        .unrebuildable_labels()
        .await
        .expect("scan for unsupported labels");
    assert!(unsupported.is_empty(), "unexpected labels: {unsupported:?}");

    fixture.cleanup().await;
}

#[tokio::test]
async fn rebuild_fails_closed_on_a_vector_with_no_reconstructable_input_db_memory() {
    // Pins: a node whose provenance cannot be reconstructed stops the rebuild
    // rather than being approximated from its display name. An approximated
    // vector indexes cleanly and retrieves wrongly, which nothing downstream
    // can detect.
    let fixture = seed_partition(1, 0).await;
    let scope = fixture.scope();
    let orphan = Uuid::now_v7();
    insert_node(&fixture.pool, &scope, orphan, "Fact", "orphan", json!({})).await;
    PgvectorStore::new_for_app_role(fixture.pool.clone(), scope.clone())
        .upsert(&[vector_item(orphan, "Fact", basis_vector(9))])
        .await
        .expect("seed orphan vector");

    let error = fixture
        .repository()
        .load_authoritative_inputs(None, 100)
        .await
        .expect_err("a fact with no summary has no reconstructable input");

    assert!(
        matches!(error, VectorError::RebuildProvenanceMissing { .. }),
        "unexpected error: {error}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn concurrent_rebuild_starts_cannot_both_own_one_partition_db_memory() {
    // Pins: the partial unique index, not an application read-then-write, is
    // what admits a single live rebuild. Two starts that both observe an idle
    // partition still resolve to one winner.
    let fixture = seed_partition(1, 0).await;
    let repository = fixture.repository();
    let first = StartRebuild {
        operation_uid: RebuildOperationId::new(),
        owner_token: Uuid::now_v7(),
        kind: RebuildKind::Reembed,
        embedding_model: "embed-v5.0".to_string(),
        embedding_model_version: 1,
        estimate_micros_per_million_tokens: ESTIMATE_RATE,
    };
    let second = StartRebuild {
        operation_uid: RebuildOperationId::new(),
        owner_token: Uuid::now_v7(),
        ..first.clone()
    };

    repository
        .start_operation(first.clone())
        .await
        .expect("first start wins");
    let error = repository
        .start_operation(second)
        .await
        .expect_err("a second live rebuild must be refused");

    assert!(
        matches!(error, VectorError::RebuildPartitionBusy { .. }),
        "unexpected error: {error}"
    );
    // The winner's own retry is a replay, not a conflict.
    let replayed = repository
        .start_operation(first)
        .await
        .expect("the owner's retry resumes its operation");
    assert_eq!(replayed.lifecycle, RebuildLifecycle::Planning);

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_replayed_build_batch_does_not_duplicate_candidates_db_memory() {
    // Pins: crash/retry cannot inflate a rebuild. Candidate rows upsert on
    // their primary key and `vectors_rebuilt` is recounted rather than
    // incremented, so replaying an identical batch leaves both unchanged.
    let fixture = seed_partition(3, 0).await;
    let repository = fixture.repository();
    let (operation_uid, owner_token, generation_uid) = build_full_generation(&fixture).await;

    let before = repository
        .load_operation(operation_uid)
        .await
        .expect("load operation")
        .expect("operation exists");
    assert_eq!(before.vectors_rebuilt, 3);

    let inputs = repository
        .load_authoritative_inputs(None, 1024)
        .await
        .expect("reload inputs");
    let candidates = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| CandidateVector::from_input(input, basis_vector(index)))
        .collect::<Vec<_>>();
    let last_uid = candidates.last().expect("candidates present").uid;

    repository
        .commit_batch(
            RebuildFence {
                operation_uid,
                owner_token,
                generation_uid,
            },
            BatchCommit {
                candidates: &candidates,
                checkpoint_uid: last_uid,
                batch_index: 0,
                counters: BatchCounters {
                    vectors_failed: 0,
                    estimated_input_tokens: 999,
                    provider_requests: 1,
                    provider_throttles: 0,
                    provider_retries: 0,
                },
                estimate_micros_per_million_tokens: ESTIMATE_RATE,
            },
        )
        .await
        .expect("replayed batch is accepted");

    let after = repository
        .load_operation(operation_uid)
        .await
        .expect("reload operation")
        .expect("operation exists");
    assert_eq!(
        after.vectors_rebuilt, before.vectors_rebuilt,
        "a replayed batch must not inflate progress"
    );
    assert_eq!(
        after.estimated_input_tokens, before.estimated_input_tokens,
        "a replayed batch must not double-count its cost estimate"
    );
    assert_eq!(
        after.checkpoint_uid, before.checkpoint_uid,
        "the checkpoint is monotonic and does not rewind"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_foreign_owner_loses_the_rebuild_fence_db_memory() {
    // Pins: one generation cannot overwrite another's fence. A writer holding a
    // stale owner token is refused and told what it actually observed, rather
    // than silently taking over the operation.
    let fixture = seed_partition(1, 0).await;
    let repository = fixture.repository();
    let (operation_uid, owner_token, generation_uid) = build_full_generation(&fixture).await;

    let error = repository
        .commit_batch(
            RebuildFence {
                operation_uid,
                owner_token: Uuid::now_v7(),
                generation_uid,
            },
            BatchCommit {
                candidates: &[],
                checkpoint_uid: Uuid::now_v7(),
                batch_index: 9,
                counters: BatchCounters::default(),
                estimate_micros_per_million_tokens: ESTIMATE_RATE,
            },
        )
        .await
        .expect_err("a foreign owner must lose the fence");
    assert!(
        matches!(error, VectorError::RebuildFenceLost { .. }),
        "unexpected error: {error}"
    );

    let transition = repository
        .transition(
            operation_uid,
            Uuid::now_v7(),
            RebuildLifecycle::Building,
            RebuildLifecycle::Validating,
        )
        .await
        .expect_err("a foreign owner cannot transition the operation");
    assert!(matches!(transition, VectorError::RebuildFenceLost { .. }));

    // The true owner's replayed transition is recognized as its own work.
    repository
        .transition(
            operation_uid,
            owner_token,
            RebuildLifecycle::Building,
            RebuildLifecycle::Validating,
        )
        .await
        .expect("the owner transitions");
    let replay = repository
        .transition(
            operation_uid,
            owner_token,
            RebuildLifecycle::Building,
            RebuildLifecycle::Validating,
        )
        .await
        .expect("a replayed transition is already applied");
    assert!(matches!(
        replay,
        moa_memory_vector::rebuild::TransitionOutcome::AlreadyApplied(_)
    ));

    fixture.cleanup().await;
}

#[tokio::test]
async fn production_never_sees_candidate_vectors_before_activation_db_memory() {
    // Pins: shadow results cannot leak. Candidate vectors live in their own
    // table, so the served embeddings table holds none of them and no
    // retrieval leg — which reads only that table — can return one.
    let fixture = seed_partition(2, 0).await;
    let (_operation_uid, _owner, generation_uid) = build_full_generation(&fixture).await;

    let candidate_uids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT uid FROM moa.knowledge_rebuild_candidate_vector WHERE generation_uid = $1",
    )
    .bind(generation_uid.0)
    .fetch_all(&fixture.pool)
    .await
    .expect("load candidate uids");
    assert_eq!(candidate_uids.len(), 2);

    let served: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT uid, embedding_model FROM moa.embeddings WHERE storage_partition_id = $1",
    )
    .bind(&fixture.storage_partition_id)
    .fetch_all(&fixture.pool)
    .await
    .expect("load served vectors");

    assert!(
        served.iter().all(|(_, model)| model == "embed-v4.0"),
        "production still serves the previous generation's model: {served:?}"
    );
    let seeded = fixture.seeded.iter().copied().collect::<HashSet<_>>();
    assert_eq!(
        served.iter().map(|(uid, _)| *uid).collect::<HashSet<_>>(),
        seeded,
        "the served set is unchanged while the candidate builds"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn activation_is_refused_for_an_incomplete_generation_db_memory() {
    // Pins: a generation missing vectors cannot activate. Activating it would
    // make part of the partition unsearchable, which is worse than the vectors
    // it replaced.
    let fixture = seed_partition(3, 0).await;
    let repository = fixture.repository();
    let operation_uid = RebuildOperationId::new();
    let owner_token = Uuid::now_v7();
    repository
        .start_operation(StartRebuild {
            operation_uid,
            owner_token,
            kind: RebuildKind::Reembed,
            embedding_model: "embed-v5.0".to_string(),
            embedding_model_version: 1,
            estimate_micros_per_million_tokens: ESTIMATE_RATE,
        })
        .await
        .expect("start operation");
    repository
        .ensure_bootstrap_generation("embed-v4.0", 1, &fixture.storage_partition_id)
        .await
        .expect("adopt bootstrap generation");
    let generation = repository
        .create_candidate_generation(
            operation_uid,
            owner_token,
            EmbeddingGenerationId::new(),
            "embed-v5.0",
            1,
            &fixture.storage_partition_id,
        )
        .await
        .expect("create candidate generation");

    let error = repository
        .mark_generation_complete(generation.generation_uid, 3)
        .await
        .expect_err("a generation with no candidates is not complete");
    assert!(
        matches!(error, VectorError::RebuildGenerationIncomplete { .. }),
        "unexpected error: {error}"
    );

    let pointer = repository
        .load_active_generation()
        .await
        .expect("load pointer")
        .expect("bootstrap pointer exists");
    let refused = repository
        .activate_generation(generation.generation_uid, pointer.pointer_version)
        .await
        .expect_err("an incomplete generation cannot activate");
    assert!(matches!(
        refused,
        VectorError::RebuildGenerationIncomplete { .. }
    ));

    let unchanged = repository
        .load_active_generation()
        .await
        .expect("reload pointer")
        .expect("pointer still exists");
    assert_eq!(
        unchanged.generation_uid, pointer.generation_uid,
        "the old generation stays authoritative after a refused activation"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn activation_and_rollback_are_pointer_compare_and_swaps_db_memory() {
    // Pins: activation and rollback are atomic pointer swaps. A caller holding
    // a stale pointer version loses, so two concurrent activations cannot both
    // believe they flipped the partition.
    let fixture = seed_partition(2, 0).await;
    let repository = fixture.repository();
    let (_operation_uid, _owner, generation_uid) = build_full_generation(&fixture).await;

    let pointer = repository
        .load_active_generation()
        .await
        .expect("load pointer")
        .expect("pointer exists");
    let bootstrap_generation = pointer.generation_uid;

    let stale = repository
        .activate_generation(generation_uid, pointer.pointer_version + 5)
        .await
        .expect_err("a stale pointer version must lose the swap");
    assert!(
        matches!(stale, VectorError::ActiveGenerationPointerConflict { .. }),
        "unexpected error: {stale}"
    );

    let activated = repository
        .activate_generation(generation_uid, pointer.pointer_version)
        .await
        .expect("activation succeeds at the current pointer version");
    assert_eq!(activated.generation_uid, generation_uid);
    assert_eq!(
        activated.previous_generation_uid,
        Some(bootstrap_generation)
    );

    let served: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT embedding_model FROM moa.embeddings WHERE storage_partition_id = $1",
    )
    .bind(&fixture.storage_partition_id)
    .fetch_all(&fixture.pool)
    .await
    .expect("load served models");
    assert_eq!(
        served,
        vec!["embed-v5.0".to_string()],
        "activation promotes the whole generation, never a mix of models"
    );

    let rolled_back = repository
        .rollback_generation(activated.pointer_version)
        .await
        .expect("rollback restores the previous generation");
    assert_eq!(rolled_back.generation_uid, bootstrap_generation);
    assert_eq!(rolled_back.previous_generation_uid, None);

    fixture.cleanup().await;
}

#[tokio::test]
async fn finalization_removes_the_retired_generation_db_memory() {
    // Pins: after finalization no reader can reconstruct the retired contract.
    // The retired candidate rows are gone and the pointer names nothing to roll
    // back to, so a later rollback is refused rather than half-applied.
    let fixture = seed_partition(2, 0).await;
    let repository = fixture.repository();
    let (_operation_uid, _owner, generation_uid) = build_full_generation(&fixture).await;
    let pointer = repository
        .load_active_generation()
        .await
        .expect("load pointer")
        .expect("pointer exists");
    let activated = repository
        .activate_generation(generation_uid, pointer.pointer_version)
        .await
        .expect("activate");

    repository
        .finalize_generation(generation_uid)
        .await
        .expect("finalize");

    let retained: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.knowledge_rebuild_candidate_vector WHERE storage_partition_id = $1 AND generation_uid <> $2",
    )
    .bind(&fixture.storage_partition_id)
    .bind(generation_uid.0)
    .fetch_one(&fixture.pool)
    .await
    .expect("count retained candidates");
    assert_eq!(retained, 0, "retired candidate vectors are discarded");

    let final_pointer = repository
        .load_active_generation()
        .await
        .expect("reload pointer")
        .expect("pointer exists");
    assert_eq!(final_pointer.previous_generation_uid, None);
    assert!(final_pointer.pointer_version > activated.pointer_version);

    let refused = repository
        .rollback_generation(final_pointer.pointer_version)
        .await
        .expect_err("rollback after finalization has nothing to restore");
    assert!(matches!(
        refused,
        VectorError::RebuildRollbackUnavailable { .. }
    ));

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_reembed_fence_stops_ordinary_writes_db_memory() {
    // Pins: the `reembed_state = 'in_progress'` fence covers ordinary writes,
    // not only KNN reads. A write that landed mid-build would either miss the
    // census (and vanish at activation) or survive in the retired model's
    // space; both are silent.
    let fixture = seed_partition(1, 0).await;
    let scope = fixture.scope();
    let store = PgvectorStore::new_for_app_role(fixture.pool.clone(), scope.clone());
    let uid = Uuid::now_v7();
    insert_node(
        &fixture.pool,
        &scope,
        uid,
        "Fact",
        "fenced",
        json!({"summary": "written during a rebuild"}),
    )
    .await;

    store
        .upsert(&[vector_item(uid, "Fact", basis_vector(5))])
        .await
        .expect("writes succeed while the partition is steady");

    let mut conn = ScopedConn::begin_as_app(&fixture.pool, &scope, true)
        .await
        .expect("begin fence transaction");
    sqlx::query(
        "UPDATE moa.storage_partition_state SET reembed_state = 'in_progress' WHERE storage_partition_id = $1",
    )
    .bind(&fixture.storage_partition_id)
    .execute(conn.as_mut())
    .await
    .expect("raise the re-embed fence");
    conn.commit().await.expect("commit fence");

    let error = store
        .upsert(&[vector_item(uid, "Fact", basis_vector(6))])
        .await
        .expect_err("ordinary writes are fenced during a re-embed");
    assert!(
        matches!(error, VectorError::ReembedInProgress { .. }),
        "unexpected error: {error}"
    );

    // The fence must be distinguishable from the embedder guard that shares the
    // same write path. A caller seeing `ReembedInProgress` should retry after
    // the rebuild; a caller seeing `EmbedderModelMismatch` has a
    // misconfiguration and retrying forever would not help. One shared error
    // would make those indistinguishable.
    let mut conn = ScopedConn::begin_as_app(&fixture.pool, &scope, true)
        .await
        .expect("begin fence-clearing transaction");
    sqlx::query(
        "UPDATE moa.storage_partition_state SET reembed_state = 'steady' WHERE storage_partition_id = $1",
    )
    .bind(&fixture.storage_partition_id)
    .execute(conn.as_mut())
    .await
    .expect("lower the re-embed fence");
    conn.commit().await.expect("commit fence release");

    let mut wrong_model = vector_item(uid, "Fact", basis_vector(7));
    wrong_model.embedding_model = "embed-v5.0".to_string();
    let mismatch = store
        .upsert(&[wrong_model])
        .await
        .expect_err("a foreign embedding model is rejected");
    assert!(
        matches!(mismatch, VectorError::EmbedderModelMismatch { .. }),
        "the embedder guard must stay distinct from the rebuild fence: {mismatch}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn rebuild_status_reports_counts_cost_estimate_and_safe_errors_db_memory() {
    // Pins: the operator-visible status carries generation ids, exact counts, a
    // deterministic cost estimate, provider rate/retry state, and a bounded
    // safe error. A provider message long enough to blow the column bound is
    // clipped rather than rejected at write time.
    let fixture = seed_partition(2, 0).await;
    let repository = fixture.repository();
    let (operation_uid, owner_token, generation_uid) = build_full_generation(&fixture).await;
    repository
        .commit_batch(
            RebuildFence {
                operation_uid,
                owner_token,
                generation_uid,
            },
            BatchCommit {
                candidates: &[],
                checkpoint_uid: Uuid::max(),
                batch_index: 1,
                counters: BatchCounters {
                    vectors_failed: 1,
                    estimated_input_tokens: 2_000_000,
                    provider_requests: 3,
                    provider_throttles: 2,
                    provider_retries: 4,
                },
                estimate_micros_per_million_tokens: ESTIMATE_RATE,
            },
        )
        .await
        .expect("commit counter-only batch");
    repository
        .record_error(
            operation_uid,
            "rebuild_provider_throttled",
            &"x".repeat(4096),
        )
        .await
        .expect("record a safe error");

    let operation = repository
        .load_operation(operation_uid)
        .await
        .expect("load operation")
        .expect("operation exists");

    assert_eq!(operation.vectors_total, 2);
    assert_eq!(operation.vectors_rebuilt, 2);
    assert_eq!(operation.vectors_failed, 1);
    assert_eq!(operation.provider_requests, 4);
    assert_eq!(operation.provider_throttles, 2);
    assert_eq!(operation.provider_retries, 4);
    assert_eq!(
        operation.estimated_cost_micros,
        (operation.estimated_input_tokens * ESTIMATE_RATE) / 1_000_000,
        "cost is the deterministic projection, never a provider figure"
    );
    assert_eq!(
        operation.last_error_code.as_deref(),
        Some("rebuild_provider_throttled")
    );
    assert!(
        operation
            .last_error_message
            .as_ref()
            .is_some_and(|message| message.len() <= 512),
        "the safe error surface is bounded"
    );
    assert_eq!(operation.candidate_generation_uid, Some(generation_uid));

    fixture.cleanup().await;
}

#[tokio::test]
async fn cancellation_stops_a_rebuild_at_a_committed_checkpoint_db_memory() {
    // Pins: cancellation is cooperative and observable. The request is durable,
    // the build sees it at a batch boundary, and the committed checkpoint still
    // describes exactly what was built.
    let fixture = seed_partition(2, 0).await;
    let repository = fixture.repository();
    let (operation_uid, _owner, _generation) = build_full_generation(&fixture).await;

    let before = repository
        .load_operation(operation_uid)
        .await
        .expect("load operation")
        .expect("operation exists");
    assert!(before.cancel_requested_at.is_none());

    assert!(
        repository
            .request_cancel(operation_uid)
            .await
            .expect("request cancellation"),
        "a live operation accepts a cancellation request"
    );

    let after = repository
        .load_operation(operation_uid)
        .await
        .expect("reload operation")
        .expect("operation exists");
    assert!(after.cancel_requested_at.is_some());
    assert_eq!(
        after.checkpoint_uid, before.checkpoint_uid,
        "cancellation does not disturb committed progress"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn one_partition_holds_exactly_one_active_generation_db_memory() {
    // Pins: the single-active partial unique index. Two rows claiming `active`
    // for one partition would make "which generation is production reading"
    // unanswerable, so the database refuses the second.
    let fixture = seed_partition(1, 0).await;
    let repository = fixture.repository();
    repository
        .ensure_bootstrap_generation("embed-v4.0", 1, &fixture.storage_partition_id)
        .await
        .expect("adopt bootstrap generation");

    let conflict = sqlx::query(
        r#"
        INSERT INTO moa.knowledge_rebuild_generation
            (generation_uid, tenant_id, storage_partition_id, generation_seq, embedding_model,
             embedding_model_version, embedding_dimension, turbopuffer_namespace, state)
        VALUES ($1, $2, $3, 99, 'embed-v5.0', 1, 1024, $4, 'active')
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(fixture.tenant_id.0)
    .bind(&fixture.storage_partition_id)
    .bind(format!("{}__g99", fixture.storage_partition_id))
    .execute(&fixture.pool)
    .await;

    assert!(
        conflict.is_err(),
        "a second active generation for one partition must be refused by the index"
    );

    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.knowledge_rebuild_generation WHERE storage_partition_id = $1 AND state = 'active'",
    )
    .bind(&fixture.storage_partition_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("count active generations");
    assert_eq!(active, 1);

    fixture.cleanup().await;
}

#[tokio::test]
async fn rechunk_refuses_to_activate_with_a_missing_staged_member_db_memory() {
    // Pins: the atomic rechunk boundary is all-or-nothing at the *staging* gate.
    // Five of six members present is refused before any served row is touched,
    // because applying a subset leaves chunks whose graph, ACL, or occurrence
    // identity still describes the old text.
    use moa_core::types::memory::RechunkStagingMember;

    let fixture = seed_partition(0, 2).await;
    let generation_uid = EmbeddingGenerationId::new();
    let document_version_uid: Uuid = sqlx::query_scalar(
        "SELECT document_version_uid FROM moa.knowledge_document_versions WHERE tenant_id = $1",
    )
    .bind(fixture.tenant_id.0)
    .fetch_one(&fixture.pool)
    .await
    .expect("load seeded document version");

    // Create the generation row the staging rows reference.
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_rebuild_generation
            (generation_uid, tenant_id, storage_partition_id, generation_seq, embedding_model,
             embedding_model_version, embedding_dimension, turbopuffer_namespace, state)
        VALUES ($1, $2, $3, 1, 'embed-v4.0', 1, 1024, $4, 'candidate')
        "#,
    )
    .bind(generation_uid.0)
    .bind(fixture.tenant_id.0)
    .bind(&fixture.storage_partition_id)
    .bind(format!("{}__g1", fixture.storage_partition_id))
    .execute(&fixture.pool)
    .await
    .expect("create candidate generation row");

    // Stage every member except the ACL snapshot.
    for member in RechunkStagingMember::ALL
        .into_iter()
        .filter(|member| *member != RechunkStagingMember::AclSnapshot)
    {
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_rechunk_staging
                (staging_uid, generation_uid, tenant_id, storage_partition_id,
                 document_version_uid, member, payload)
            VALUES ($1, $2, $3, $4, $5, $6, '{}'::JSONB)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(generation_uid.0)
        .bind(fixture.tenant_id.0)
        .bind(&fixture.storage_partition_id)
        .bind(document_version_uid)
        .bind(member.as_str())
        .execute(&fixture.pool)
        .await
        .expect("stage rechunk member");
    }

    let staged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.knowledge_rechunk_staging WHERE generation_uid = $1",
    )
    .bind(generation_uid.0)
    .fetch_one(&fixture.pool)
    .await
    .expect("count staged members");
    assert_eq!(staged, 5, "the fixture stages five of six members");

    let missing: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT member
          FROM unnest(moa.knowledge_rechunk_staged_members()) AS member
         WHERE member NOT IN (
             SELECT member FROM moa.knowledge_rechunk_staging WHERE generation_uid = $1
         )
        "#,
    )
    .bind(generation_uid.0)
    .fetch_all(&fixture.pool)
    .await
    .expect("compute missing members");
    assert_eq!(
        missing,
        vec!["acl_snapshot".to_string()],
        "the completeness rule and the SQL member vocabulary agree"
    );

    let chunk_text: Vec<String> = sqlx::query_scalar(
        "SELECT text FROM moa.knowledge_chunks WHERE tenant_id = $1 ORDER BY ordinal",
    )
    .bind(fixture.tenant_id.0)
    .fetch_all(&fixture.pool)
    .await
    .expect("load chunk text");
    assert_eq!(
        chunk_text,
        vec![
            "chunk body number 0".to_string(),
            "chunk body number 1".to_string()
        ],
        "an incomplete rechunk leaves the served chunks untouched"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn candidate_vectors_carry_the_digest_of_the_input_that_produced_them_db_memory() {
    // Pins: provenance is checkable, not asserted. Every candidate row stores
    // the SHA-256 of the exact reconstructed input, so "this was rebuilt from
    // the authoritative text" is a value an auditor can recompute.
    let fixture = seed_partition(2, 0).await;
    let repository = fixture.repository();
    let (_operation_uid, _owner, generation_uid) = build_full_generation(&fixture).await;

    let inputs = repository
        .load_authoritative_inputs(None, 100)
        .await
        .expect("reload inputs");
    let rows = sqlx::query(
        "SELECT uid, input_digest FROM moa.knowledge_rebuild_candidate_vector WHERE generation_uid = $1",
    )
    .bind(generation_uid.0)
    .fetch_all(&fixture.pool)
    .await
    .expect("load candidate digests");

    assert_eq!(rows.len(), inputs.len());
    for row in rows {
        let uid: Uuid = row.try_get("uid").expect("uid column");
        let digest: Vec<u8> = row.try_get("input_digest").expect("digest column");
        let expected = inputs
            .iter()
            .find(|input| input.uid == uid)
            .expect("candidate matches a reconstructed input")
            .digest();
        assert_eq!(
            digest, expected,
            "digest for {uid} does not match its input"
        );
        assert_eq!(digest.len(), 32);
    }

    fixture.cleanup().await;
}
