//! Bounded tenant-purge catalog and execution scenarios.

use super::support::*;

async fn assert_tenant_purge_graph_scope_uses_typed_tenant(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tenant_id = uuid::Uuid::new_v4();
    let neighbor_tenant_id = uuid::Uuid::new_v4();
    let operation_id = format!("tenant-purge-graph-{tenant_id}");
    let opaque_partition = format!("opaque-graph-{tenant_id}");
    let neighbor_partition = format!("opaque-graph-{neighbor_tenant_id}");
    let first_node = uuid::Uuid::new_v4();
    let second_node = uuid::Uuid::new_v4();
    let edge_id = uuid::Uuid::new_v4();

    sqlx::query(
        "INSERT INTO tenants (id, slug, name) VALUES \
         ($1, $2, 'bounded tenant purge opaque graph target'), \
         ($3, $4, 'bounded tenant purge opaque graph neighbor')",
    )
    .bind(tenant_id)
    .bind(format!("tenant-purge-opaque-target-{tenant_id}"))
    .bind(neighbor_tenant_id)
    .bind(format!("tenant-purge-opaque-neighbor-{neighbor_tenant_id}"))
    .execute(pool)
    .await?;

    let mut graph_write = pool.begin().await?;
    sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *graph_write)
        .await?;
    sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, storage_partition_id, tenant_id, data_subject_id, name, pii_class) \
         VALUES ($1, 'Fact', $2, $3, $3, 'opaque fact one', 'none'), \
                ($4, 'Fact', $2, $3, $3, 'opaque fact two', 'none')",
    )
    .bind(first_node)
    .bind(&opaque_partition)
    .bind(tenant_id)
    .bind(second_node)
    .execute(&mut *graph_write)
    .await?;
    sqlx::query(
        "INSERT INTO moa.edge_index \
            (uid, label, start_uid, end_uid, storage_partition_id, tenant_id) \
         VALUES ($1, 'RELATES_TO', $2, $3, $4, $5)",
    )
    .bind(edge_id)
    .bind(first_node)
    .bind(second_node)
    .bind(&opaque_partition)
    .bind(tenant_id)
    .execute(&mut *graph_write)
    .await?;
    let zero_embedding = format!("[{}]", vec!["0"; 1024].join(","));
    sqlx::query(
        "INSERT INTO moa.embeddings \
            (uid, storage_partition_id, tenant_id, label, pii_class, embedding, \
             embedding_model, embedding_model_version) \
         VALUES ($1, $2, $3, 'Fact', 'none', $4::public.halfvec, 'test', 1)",
    )
    .bind(first_node)
    .bind(&opaque_partition)
    .bind(tenant_id)
    .bind(&zero_embedding)
    .execute(&mut *graph_write)
    .await?;
    sqlx::query(
        "INSERT INTO moa.graph_changelog \
            (storage_partition_id, tenant_id, actor_id, actor_kind, op, \
             target_kind, target_label, target_uid, payload) \
         VALUES ($1, $2, 'tenant-purge-test', 'system', 'create', 'node', 'Fact', $3, '{}'::JSONB)",
    )
    .bind(&opaque_partition)
    .bind(tenant_id)
    .bind(first_node)
    .execute(&mut *graph_write)
    .await?;
    graph_write.commit().await?;

    let seeded_graph_rows: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM moa.embeddings WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.graph_changelog WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.edge_index WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.node_index WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.storage_partition_state WHERE tenant_id = $1)",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(seeded_graph_rows, (1, 1, 1, 2, 1));

    let storage_only_error = sqlx::query(
        "INSERT INTO moa.vector_sync_outbox (storage_partition_id, uid, op) \
         VALUES ($1, $2, 'delete')",
    )
    .bind(format!("opaque-vector-{tenant_id}"))
    .bind(uuid::Uuid::new_v4())
    .execute(pool)
    .await
    .expect_err("true storage-only scope must fail closed for a non-UUID value");
    let storage_only_sqlstate = storage_only_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    assert_eq!(storage_only_sqlstate.as_deref(), Some("22P02"));

    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&operation_id)
        .execute(pool)
        .await?;

    let fenced_node = uuid::Uuid::new_v4();
    let atomic_neighbor_node = uuid::Uuid::new_v4();
    let fenced_insert_error = sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, storage_partition_id, tenant_id, data_subject_id, name, pii_class) \
         VALUES ($1, 'Fact', $2, $3, $3, 'must roll back', 'none'), \
                ($4, 'Fact', $5, $6, $6, 'neighbor must roll back atomically', 'none')",
    )
    .bind(fenced_node)
    .bind(&opaque_partition)
    .bind(tenant_id)
    .bind(atomic_neighbor_node)
    .bind(&neighbor_partition)
    .bind(neighbor_tenant_id)
    .execute(pool)
    .await
    .expect_err("a typed fenced tenant in a multirow graph write must reject the statement");
    let fenced_insert_sqlstate = fenced_insert_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    assert_eq!(fenced_insert_sqlstate.as_deref(), Some("55000"));
    let atomic_insert_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moa.node_index WHERE uid = ANY($1)")
            .bind(vec![fenced_node, atomic_neighbor_node])
            .fetch_one(pool)
            .await?;
    assert_eq!(
        atomic_insert_count, 0,
        "the rejected statement must be atomic"
    );

    sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, storage_partition_id, tenant_id, data_subject_id, name, pii_class) \
         VALUES ($1, 'Fact', $2, $3, $3, 'writable neighbor', 'none')",
    )
    .bind(atomic_neighbor_node)
    .bind(&neighbor_partition)
    .bind(neighbor_tenant_id)
    .execute(pool)
    .await?;
    let fenced_update_error = sqlx::query(
        "UPDATE moa.node_index SET tenant_id = $1, name = 'must not move' WHERE uid = $2",
    )
    .bind(tenant_id)
    .bind(atomic_neighbor_node)
    .execute(pool)
    .await
    .expect_err("UPDATE must derive both old and new typed tenant identities");
    let fenced_update_sqlstate = fenced_update_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    assert_eq!(fenced_update_sqlstate.as_deref(), Some("55000"));
    let neighbor_after_update: (uuid::Uuid, String) =
        sqlx::query_as("SELECT tenant_id, name FROM moa.node_index WHERE uid = $1")
            .bind(atomic_neighbor_node)
            .fetch_one(pool)
            .await?;
    assert_eq!(
        neighbor_after_update,
        (neighbor_tenant_id, "writable neighbor".to_string())
    );
    sqlx::query("UPDATE moa.node_index SET name = 'neighbor updated' WHERE uid = $1")
        .bind(atomic_neighbor_node)
        .execute(pool)
        .await?;

    sqlx::query(
        "UPDATE moa.tenant_purge_operations \
         SET current_stage = 'moa.embeddings' \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .execute(pool)
    .await?;
    let mut positive_batches = Vec::new();
    for _ in 0..16 {
        let batch: (String, String, i64) = sqlx::query_as(
            "SELECT batch_state, stage, affected \
             FROM moa.run_tenant_purge_batch($1, $2)",
        )
        .bind(tenant_id)
        .bind(&operation_id)
        .fetch_one(pool)
        .await?;
        if batch.2 > 0 {
            positive_batches.push((batch.1.clone(), batch.2));
        }
        if batch.1 == "public.session_event_dedupe" {
            break;
        }
    }
    assert_eq!(
        positive_batches,
        vec![
            ("moa.embeddings".to_string(), 1),
            ("moa.graph_changelog".to_string(), 1),
            ("moa.edge_index".to_string(), 1),
            ("moa.node_index".to_string(), 2),
            ("moa.storage_partition_state".to_string(), 1),
        ]
    );
    let graph_residue: (i64, i64, i64, i64, i64, i64, String) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM moa.embeddings WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.graph_changelog WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.edge_index WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.node_index WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.storage_partition_state WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.node_index WHERE tenant_id = $2), \
            (SELECT name FROM moa.node_index WHERE uid = $3)",
    )
    .bind(tenant_id)
    .bind(neighbor_tenant_id)
    .bind(atomic_neighbor_node)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        graph_residue,
        (0, 0, 0, 0, 0, 1, "neighbor updated".to_string())
    );

    Ok(())
}

async fn assert_tenant_purge_purge_index_catalog(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Pins: every bounded candidate query has a valid, ready index whose first
    // key is the purge scope; nullable scopes and retained legal holds stay narrow.
    for (expected_schema, index, expected_table, expected_key, expected_predicate) in
        TENANT_PURGE_SCOPE_INDEXES
    {
        let actual: (String, String, bool, bool, String, Option<String>) = sqlx::query_as(
            r#"
            SELECT table_namespace.nspname,
                   table_relation.relname,
                   index_row.indisvalid,
                   index_row.indisready,
                   pg_get_indexdef(index_row.indexrelid, 1, TRUE),
                   pg_get_expr(index_row.indpred, index_row.indrelid)
            FROM pg_index AS index_row
            JOIN pg_class AS index_relation ON index_relation.oid = index_row.indexrelid
            JOIN pg_namespace AS index_namespace
              ON index_namespace.oid = index_relation.relnamespace
            JOIN pg_class AS table_relation ON table_relation.oid = index_row.indrelid
            JOIN pg_namespace AS table_namespace
              ON table_namespace.oid = table_relation.relnamespace
            WHERE index_namespace.nspname = $1
              AND index_relation.relname = $2
            "#,
        )
        .bind(expected_schema)
        .bind(index)
        .fetch_one(pool)
        .await?;

        if actual.0 != expected_schema
            || actual.1 != expected_table
            || !actual.2
            || !actual.3
            || actual.4 != expected_key
        {
            return Err(std::io::Error::other(format!(
                "purge index {index} is not a ready leading {expected_schema}.{expected_table}({expected_key}) path: {actual:?}"
            ))
            .into());
        }
        match expected_predicate {
            None if actual.5.is_some() => {
                return Err(std::io::Error::other(format!(
                    "purge index {index} unexpectedly has predicate {:?}",
                    actual.5
                ))
                .into());
            }
            Some(fragment)
                if !actual
                    .5
                    .as_deref()
                    .is_some_and(|predicate| predicate.contains(fragment)) =>
            {
                return Err(std::io::Error::other(format!(
                    "purge index {index} is missing predicate fragment {fragment}: {:?}",
                    actual.5
                ))
                .into());
            }
            _ => {}
        }
        if index == "tenant_purge_legal_hold_idx"
            && ![
                "subject_id IS NOT NULL",
                "reason <> '[REDACTED]'::text",
                "placed_by <> '[REDACTED]'::text",
                "released_by <> '[REDACTED]'::text",
            ]
            .iter()
            .all(|fragment| {
                actual
                    .5
                    .as_deref()
                    .is_some_and(|predicate| predicate.contains(fragment))
            })
        {
            return Err(std::io::Error::other(format!(
                "legal-hold purge index is broader than the released/redactable candidate set: {:?}",
                actual.5
            ))
            .into());
        }
    }

    let embedding_children: (i64, bool, bool, bool) = sqlx::query_as(
        r#"
        SELECT count(*),
               bool_and(child_index.indisvalid),
               bool_and(child_index.indisready),
               bool_and(pg_get_indexdef(child_index.indexrelid, 1, TRUE) = 'tenant_id')
        FROM pg_class AS parent_relation
        JOIN pg_namespace AS parent_namespace
          ON parent_namespace.oid = parent_relation.relnamespace
        JOIN pg_inherits AS attachment ON attachment.inhparent = parent_relation.oid
        JOIN pg_index AS child_index ON child_index.indexrelid = attachment.inhrelid
        WHERE parent_namespace.nspname = 'moa'
          AND parent_relation.relname = 'tenant_purge_embeddings_idx'
        "#,
    )
    .fetch_one(pool)
    .await?;
    if embedding_children != (32, true, true, true) {
        return Err(std::io::Error::other(format!(
            "partitioned embeddings purge index must attach 32 valid ready tenant-leading child paths: {embedding_children:?}"
        ))
        .into());
    }

    let authz_index: (bool, bool, String, String, Option<String>) = sqlx::query_as(
        r#"
        SELECT index_row.indisvalid,
               index_row.indisready,
               pg_get_indexdef(index_row.indexrelid, 1, TRUE),
               pg_get_indexdef(index_row.indexrelid, 2, TRUE),
               pg_get_expr(index_row.indpred, index_row.indrelid)
        FROM pg_index AS index_row
        JOIN pg_class AS index_relation ON index_relation.oid = index_row.indexrelid
        JOIN pg_namespace AS index_namespace
          ON index_namespace.oid = index_relation.relnamespace
        WHERE index_namespace.nspname = 'public'
          AND index_relation.relname = 'idx_authz_outbox_tenant'
        "#,
    )
    .fetch_one(pool)
    .await?;
    if !authz_index.0
        || !authz_index.1
        || authz_index.2 != "tenant_id"
        || authz_index.3 != "id"
        || !authz_index
            .4
            .as_deref()
            .is_some_and(|predicate| predicate.contains("tenant_id IS NOT NULL"))
    {
        return Err(std::io::Error::other(format!(
            "authz purge index must be the valid ready partial (tenant_id, id) path: {authz_index:?}"
        ))
        .into());
    }

    let index_presence: (i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*) FILTER (
                   WHERE relation.relname IN (
                       'scim_group_members_user_idx',
                       'scim_group_members_group_idx'
                   )
               ),
               count(*) FILTER (
                   WHERE relation.relname IN (
                       'auth0_ciba_approvals_session_idx',
                       'auth0_ciba_approvals_deciding_user_idx'
                   )
               )
        FROM pg_class AS relation
        JOIN pg_namespace AS namespace ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname IN ('public', 'moa')
          AND relation.relkind IN ('i', 'I')
        "#,
    )
    .fetch_one(pool)
    .await?;
    if index_presence != (0, 2) {
        return Err(std::io::Error::other(format!(
            "bounded tenant purge redundant/CIBA index presence is wrong: {index_presence:?}"
        ))
        .into());
    }

    Ok(())
}

async fn assert_tenant_purge_function_arity_and_tenant_attribution(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Pins: bounded tenant purge is a hard two-argument API break with no callable legacy overload.
    let function_arities: (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT \
            to_regprocedure('moa.invert_tenant_authz_batch(uuid,text)') IS NOT NULL, \
            to_regprocedure('moa.invert_tenant_authz_batch(uuid,text,integer)') IS NOT NULL, \
            to_regprocedure('moa.run_tenant_purge_batch(uuid,text)') IS NOT NULL, \
            to_regprocedure('moa.run_tenant_purge_batch(uuid,text,integer)') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    if function_arities != (true, false, true, false) {
        return Err(std::io::Error::other(format!(
            "bounded tenant purge purge function arities are not an exact two-argument hard break: {function_arities:?}"
        ))
        .into());
    }

    // Pins: ON CONFLICT may never move an existing tuple identity between tenants.
    let original_tenant = uuid::Uuid::new_v4();
    let conflicting_tenant = uuid::Uuid::new_v4();
    let tuple_user = format!("operator:{}", uuid::Uuid::new_v4());
    let tuple_object = format!("tenant:{original_tenant}");
    sqlx::query(
        "INSERT INTO authz_outbox \
            (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id) \
         VALUES ('write', $1, 'operator', $2, 5, $3)",
    )
    .bind(&tuple_user)
    .bind(&tuple_object)
    .bind(original_tenant)
    .execute(pool)
    .await?;
    let conflict = sqlx::query(
        "INSERT INTO authz_outbox \
            (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id) \
         VALUES ('delete', $1, 'operator', $2, 5, $3) \
         ON CONFLICT (tuple_user, tuple_relation, tuple_object, model_version) DO UPDATE \
         SET tenant_id = EXCLUDED.tenant_id",
    )
    .bind(&tuple_user)
    .bind(&tuple_object)
    .bind(conflicting_tenant)
    .execute(pool)
    .await
    .expect_err("cross-tenant ON CONFLICT attribution must fail closed");
    let conflict_sqlstate = conflict
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    if conflict_sqlstate.as_deref() != Some("55000") {
        return Err(std::io::Error::other(format!(
            "cross-tenant ON CONFLICT returned {conflict_sqlstate:?}, expected 55000"
        ))
        .into());
    }
    let unchanged: (uuid::Uuid, String, String, i64) = sqlx::query_as(
        "SELECT tenant_id, op, status, generation \
         FROM authz_outbox \
         WHERE tuple_user = $1 AND tuple_relation = 'operator' \
           AND tuple_object = $2 AND model_version = 5",
    )
    .bind(&tuple_user)
    .bind(&tuple_object)
    .fetch_one(pool)
    .await?;
    if unchanged
        != (
            original_tenant,
            "write".to_string(),
            "pending".to_string(),
            1,
        )
    {
        return Err(std::io::Error::other(format!(
            "cross-tenant ON CONFLICT changed the original outbox row: {unchanged:?}"
        ))
        .into());
    }
    let trigger_source: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef('moa.guard_authz_outbox_during_tenant_purge()'::REGPROCEDURE)",
    )
    .fetch_one(pool)
    .await?;
    if !trigger_source.contains("NEW.tenant_id IS DISTINCT FROM OLD.tenant_id") {
        return Err(std::io::Error::other(
            "authz outbox trigger source no longer protects immutable tenant attribution",
        )
        .into());
    }

    Ok(())
}

async fn seed_tenant_purge_release_policies(
    pool: &PgPool,
    tenant_id: uuid::Uuid,
    count: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        WITH policies AS (
            SELECT
                gen_random_uuid() AS policy_uid,
                $1::TEXT AS storage_partition_id,
                format('tenant-purge-bounded-%s-%s', $1::TEXT, ordinal) AS name,
                ordinal AS revision,
                '[{"id":"target_completed","version":"v1"}]'::JSONB
                    AS blocking_assertions,
                '[{"metric":"target_completed"}]'::JSONB AS primary_gate_family,
                3600::BIGINT AS attestation_ttl_secs,
                digest(format('tenant-purge-resource-%s-%s', $1::TEXT, ordinal), 'sha256')
                    AS resource_policy_hash
            FROM generate_series(1, $2::INT) AS ordinal
        )
        INSERT INTO moa.artifact_release_policy (
            policy_uid, storage_partition_id, user_id, name, revision, target_class,
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash, policy_hash, valid_to
        )
        SELECT
            policy_uid, storage_partition_id, NULL, name, revision, 'skill_visibility',
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash,
            moa.artifact_release_policy_content_hash(
                name, revision, 'skill_visibility', blocking_assertions,
                primary_gate_family, attestation_ttl_secs, resource_policy_hash
            ),
            now()
        FROM policies
        "#,
    )
    .bind(tenant_id)
    .bind(count)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_tenant_purge_activated_release_chain(
    pool: &PgPool,
    tenant_id: uuid::Uuid,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let artifact_uid = uuid::Uuid::new_v4();
    let revision_uid = uuid::Uuid::new_v4();
    let policy_uid = uuid::Uuid::new_v4();
    let attestation_uid = uuid::Uuid::new_v4();
    let audit_uid = uuid::Uuid::new_v4();
    let revision_hash = vec![1_u8; 32];
    let resource_policy_hash = vec![2_u8; 32];
    let subject_digest = vec![3_u8; 32];
    let partition = tenant_id.to_string();
    let policy_name = format!("tenant-purge-active-{tenant_id}");

    sqlx::query(
        "INSERT INTO moa.artifact \
            (artifact_uid, tenant_id, storage_partition_id, user_id, kind, name) \
         VALUES ($1, $2, $2::TEXT, NULL, 'skill', $3)",
    )
    .bind(artifact_uid)
    .bind(tenant_id)
    .bind(format!("tenant-purge-activated-{tenant_id}"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.artifact_revision (\
            revision_uid, artifact_uid, tenant_id, storage_partition_id, user_id, definition, \
            canonical_hash, source_format, source_text, status, version\
         ) VALUES ($1, $2, $3, $3::TEXT, NULL, '{}'::JSONB, $4, 'json', ''::BYTEA, 'ready', 1)",
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(tenant_id)
    .bind(&revision_hash)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE moa.artifact SET latest_revision_uid = $1 WHERE artifact_uid = $2")
        .bind(revision_uid)
        .bind(artifact_uid)
        .execute(pool)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_release_policy (
            policy_uid, storage_partition_id, user_id, name, revision, target_class,
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash, policy_hash
        ) VALUES (
            $1, $2, NULL, $3, 1, 'skill_visibility',
            '[{"id":"target_completed","version":"v1"}]'::JSONB,
            '[{"metric":"target_completed"}]'::JSONB,
            3600, $4,
            moa.artifact_release_policy_content_hash(
                $3, 1, 'skill_visibility',
                '[{"id":"target_completed","version":"v1"}]'::JSONB,
                '[{"metric":"target_completed"}]'::JSONB,
                3600, $4
            )
        )
        "#,
    )
    .bind(policy_uid)
    .bind(&partition)
    .bind(&policy_name)
    .bind(&resource_policy_hash)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.artifact_release_candidate (\
            revision_uid, artifact_uid, storage_partition_id, user_id, activation_target, \
            target_installation_uid, subject, subject_digest, candidate_revision_hash, \
            policy_uid, policy_revision, policy_hash, slot, generation\
         ) VALUES (\
            $1, $2, $3, NULL, 'skill_visibility', NULL, '{}'::JSONB, $4, $5, \
            $6, 1, moa.artifact_release_policy_content_hash(\
                $7, 1, 'skill_visibility', \
                '[{\"id\":\"target_completed\",\"version\":\"v1\"}]'::JSONB, \
                '[{\"metric\":\"target_completed\"}]'::JSONB, 3600, $8\
            ), 'released', 1\
         )",
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(&partition)
    .bind(&subject_digest)
    .bind(&revision_hash)
    .bind(policy_uid)
    .bind(&policy_name)
    .bind(&resource_policy_hash)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.artifact_activation_attestation (\
            attestation_uid, storage_partition_id, user_id, artifact_uid, \
            candidate_revision_uid, activation_target, target_installation_uid, \
            subject_digest, verdict, run_uid, trial_uids, evidence_ids, decision, \
            policy_uid, policy_revision, policy_hash, decided_by, expires_at\
         ) VALUES (\
            $1, $2, NULL, $3, $4, 'skill_visibility', NULL, $5, 'pass', $6, \
            ARRAY[$7]::UUID[], ARRAY[$8]::UUID[], '{}'::JSONB, $9, 1, \
            moa.artifact_release_policy_content_hash(\
                $10, 1, 'skill_visibility', \
                '[{\"id\":\"target_completed\",\"version\":\"v1\"}]'::JSONB, \
                '[{\"metric\":\"target_completed\"}]'::JSONB, 3600, $11\
            ), 'tenant-purge-test', now() + interval '1 hour'\
         )",
    )
    .bind(attestation_uid)
    .bind(&partition)
    .bind(artifact_uid)
    .bind(revision_uid)
    .bind(&subject_digest)
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(policy_uid)
    .bind(&policy_name)
    .bind(&resource_policy_hash)
    .execute(pool)
    .await?;

    let mut activation = pool.begin().await?;
    sqlx::query("SELECT set_config('moa.storage_partition_id', $1, true)")
        .bind(&partition)
        .execute(&mut *activation)
        .await?;
    let affected: i64 = sqlx::query_scalar(
        "SELECT moa.apply_artifact_activation_transition(\
            $1, $2, $3, 'skill', 'skill_visibility', NULL, $4, $5, NULL, 0, \
            $6, 1, $7, 1, 'tenant-purge-test', 'activated-chain proof', now()\
         )",
    )
    .bind(audit_uid)
    .bind(&partition)
    .bind(artifact_uid)
    .bind(attestation_uid)
    .bind(&subject_digest)
    .bind(revision_uid)
    .bind(&revision_hash)
    .fetch_one(&mut *activation)
    .await?;
    assert_eq!(
        affected, 1,
        "fixture activation must move one serving pointer"
    );
    activation.commit().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn bounded_tenant_purge_final_schema_executes_bounded_batches_db() {
    // Pins: a pristine final schema persists exactly 133 purge stages, installs
    // statement fences, and advances a real purge in fixed-size batches.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let tenant_id = uuid::Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect bounded tenant purge maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create bounded tenant purge throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&operation_id)
        .execute(&target)
        .await?;
        assert_tenant_purge_purge_index_catalog(&target).await?;
        assert_tenant_purge_function_arity_and_tenant_attribution(&target).await?;
        assert_tenant_purge_graph_scope_uses_typed_tenant(&target).await?;
        let migrated: (String, String, i64, i64, i64, bool, bool) = sqlx::query_as(
            "SELECT status, current_stage, stage_deleted_count, total_deleted_count, \
                    batch_count, started_at IS NOT NULL, updated_at IS NOT NULL \
             FROM moa.tenant_purge_operations WHERE tenant_id = $1",
        )
        .bind(tenant_id)
        .fetch_one(&target)
        .await?;
        let catalog_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM moa.tenant_purge_catalog")
                .fetch_one(&target)
                .await?;
        let trigger_kinds: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT trigger_name FROM information_schema.triggers \
             WHERE trigger_name IN (\
                'moa_tenant_purge_fence_insert', \
                'moa_tenant_purge_fence_update'\
             ) ORDER BY trigger_name",
        )
        .fetch_all(&target)
        .await?;
        let global_exemptions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.tenant_purge_catalog \
             WHERE table_name IN (\
                'simulator_certification_mandate', \
                'simulator_certification_evidence_import'\
             )",
        )
        .fetch_one(&target)
        .await?;
        let fence_helper_contract: (String, bool, Vec<String>, bool, bool, bool) = sqlx::query_as(
            r#"
                SELECT owner.rolname,
                       function_row.prosecdef,
                       COALESCE(function_row.proconfig, ARRAY[]::TEXT[]),
                       NOT EXISTS (
                           SELECT 1
                           FROM aclexplode(COALESCE(
                               function_row.proacl,
                               acldefault('f', function_row.proowner)
                           )) AS function_acl
                           WHERE function_acl.grantee = 0
                             AND function_acl.privilege_type = 'EXECUTE'
                       ),
                       (
                           SELECT array_agg(grantee.rolname::TEXT ORDER BY grantee.rolname) = ARRAY[
                               'moa_app',
                               'moa_artifact_activator',
                               'moa_privacy_eraser',
                               'moa_promoter'
                           ]::TEXT[]
                           FROM aclexplode(COALESCE(
                               function_row.proacl,
                               acldefault('f', function_row.proowner)
                           )) AS function_acl
                           JOIN pg_roles grantee ON grantee.oid = function_acl.grantee
                           WHERE function_acl.privilege_type = 'EXECUTE'
                             AND grantee.rolname <> 'moa_owner'
                       ),
                       NOT guard_row.prosecdef
                FROM pg_proc function_row
                JOIN pg_namespace namespace
                  ON namespace.oid = function_row.pronamespace
                JOIN pg_roles owner ON owner.oid = function_row.proowner
                JOIN pg_proc guard_row ON guard_row.oid =
                    'moa.guard_tenant_write_statement()'::REGPROCEDURE
                WHERE namespace.nspname = 'moa'
                  AND function_row.oid = 'moa.tenant_write_fenced(uuid)'::REGPROCEDURE
                "#,
        )
        .fetch_one(&target)
        .await?;
        let restricted_fence_select_grants: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*)
            FROM (VALUES
                ('moa_artifact_activator'::NAME),
                ('moa_privacy_eraser'::NAME)
            ) AS restricted(role_name)
            WHERE has_table_privilege(
                restricted.role_name,
                'moa.destruction_operation_fence',
                'SELECT'
            )
               OR has_table_privilege(
                    restricted.role_name,
                    'moa.tenant_purge_operations',
                    'SELECT'
               )
            "#,
        )
        .fetch_one(&target)
        .await?;
        let legacy_release_cleanup: (bool, i64, i64, bool, String) = sqlx::query_as(
            r#"
            WITH legacy_tables(table_name) AS (
                VALUES
                    ('artifact_release_eval_overlay'),
                    ('artifact_release_attempt'),
                    ('artifact_release_dispatch_outbox'),
                    ('artifact_release_case_pack'),
                    ('artifact_serving_pointer'),
                    ('artifact_activation_audit'),
                    ('artifact_activation_attestation'),
                    ('artifact_release_candidate'),
                    ('artifact_release_policy')
            )
            SELECT
                to_regprocedure('moa.purge_artifact_release_partition(text)') IS NULL,
                (
                    SELECT count(*)
                    FROM pg_policies AS policy
                    JOIN legacy_tables AS legacy
                      ON legacy.table_name = policy.tablename
                    WHERE policy.schemaname = 'moa'
                      AND policy.policyname IN (
                          'artifact_release_partition_purge_read',
                          'artifact_release_partition_purge'
                      )
                ),
                (
                    SELECT count(*)
                    FROM legacy_tables AS legacy
                    WHERE has_table_privilege(
                              'moa_artifact_releaser',
                              format('moa.%I', legacy.table_name),
                              'SELECT'
                          )
                       OR has_table_privilege(
                              'moa_artifact_releaser',
                              format('moa.%I', legacy.table_name),
                              'DELETE'
                          )
                ),
                has_schema_privilege('moa_artifact_releaser', 'moa', 'USAGE'),
                pg_get_functiondef('moa.artifact_activation_audit_guard()'::REGPROCEDURE)
            "#,
        )
        .fetch_one(&target)
        .await?;
        let audit_guard_contract: (String, bool, Vec<String>, bool, bool) = sqlx::query_as(
            r#"
            SELECT owner.rolname,
                   function_row.prosecdef,
                   COALESCE(function_row.proconfig, ARRAY[]::TEXT[]),
                   NOT EXISTS (
                       SELECT 1
                       FROM aclexplode(COALESCE(
                           function_row.proacl,
                           acldefault('f', function_row.proowner)
                       )) AS function_acl
                       WHERE function_acl.grantee = 0
                         AND function_acl.privilege_type = 'EXECUTE'
                   ),
                   NOT EXISTS (
                       SELECT 1
                       FROM aclexplode(COALESCE(
                           function_row.proacl,
                           acldefault('f', function_row.proowner)
                       )) AS function_acl
                       WHERE function_acl.grantee <> function_row.proowner
                         AND function_acl.privilege_type = 'EXECUTE'
                   )
            FROM pg_proc AS function_row
            JOIN pg_roles AS owner ON owner.oid = function_row.proowner
            WHERE function_row.oid =
                'moa.artifact_activation_audit_guard()'::REGPROCEDURE
            "#,
        )
        .fetch_one(&target)
        .await?;

        let purge_tenant = uuid::Uuid::new_v4();
        let neighbor_tenant = uuid::Uuid::new_v4();
        let purge_operation = format!("tenant-purge-{purge_tenant}");
        sqlx::query(
            "INSERT INTO tenants (id, slug, name) VALUES \
             ($1, $2, 'bounded tenant purge purge target'), ($3, $4, 'bounded tenant purge neighbor')",
        )
        .bind(purge_tenant)
        .bind(format!("tenant-purge-target-{purge_tenant}"))
        .bind(neighbor_tenant)
        .bind(format!("tenant-purge-neighbor-{neighbor_tenant}"))
        .execute(&target)
        .await?;
        seed_tenant_purge_release_policies(&target, purge_tenant, 1000).await?;
        seed_tenant_purge_activated_release_chain(&target, purge_tenant).await?;
        seed_tenant_purge_activated_release_chain(&target, neighbor_tenant).await?;
        let ordinary_activation_delete = sqlx::query(
            "DELETE FROM moa.artifact_activation_audit WHERE storage_partition_id = $1::TEXT",
        )
        .bind(purge_tenant)
        .execute(&target)
        .await
        .expect_err("ordinary activation-audit deletion must remain append-only");
        let ordinary_activation_delete_sqlstate = ordinary_activation_delete
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()));
        sqlx::query(
            "INSERT INTO users (id, tenant_id, email, active) \
             SELECT gen_random_uuid(), $1, 'tenant-purge-' || ordinal || '@example.test', true \
             FROM generate_series(1, 1001) AS ordinal",
        )
        .bind(purge_tenant)
        .execute(&target)
        .await?;
        let purge_operator: uuid::Uuid = sqlx::query_scalar(
            "SELECT id FROM users WHERE tenant_id = $1 ORDER BY id LIMIT 1",
        )
        .bind(purge_tenant)
        .fetch_one(&target)
        .await?;
        let neighbour_operator = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, tenant_id, email, active) \
             VALUES ($1, $2, $1::TEXT || '@example.test', true)",
        )
        .bind(neighbour_operator)
        .bind(neighbor_tenant)
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO authz_outbox \
                (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id) \
             SELECT 'write', 'operator:' || gen_random_uuid(), 'operator', \
                    'tenant:' || $1::TEXT, 5, $1 \
             FROM generate_series(1, 1001)",
        )
        .bind(purge_tenant)
        .execute(&target)
        .await?;
        let purge_connection = uuid::Uuid::new_v4();
        let neighbour_connection = uuid::Uuid::new_v4();
        let purge_binding = uuid::Uuid::new_v4();
        let neighbour_binding = uuid::Uuid::new_v4();
        for (connection_uid, connection_tenant) in [
            (purge_connection, purge_tenant),
            (neighbour_connection, neighbor_tenant),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_connections \
                    (connection_uid, tenant_id, display_name, built_in_key, built_in_version, \
                     lifecycle_status, health_status) \
                 VALUES ($1, $2, 'purge connector', 'knowledge:nango', 1, 'active', 'ready')",
            )
            .bind(connection_uid)
            .bind(connection_tenant)
            .execute(&target)
            .await?;
        }
        for (connection_tenant, connection_uid, subject_id) in [
            (purge_tenant, purge_connection, purge_operator),
            (
                neighbor_tenant,
                neighbour_connection,
                neighbour_operator,
            ),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_connection_use_grants \
                    (tenant_id, connection_uid, subject_kind, subject_id) \
                 VALUES ($1, $2, 'operator', $3)",
            )
            .bind(connection_tenant)
            .bind(connection_uid)
            .bind(subject_id)
            .execute(&target)
            .await?;
        }
        for (binding_uid, connection_tenant, connection_uid, tool_call_id) in [
            (
                purge_binding,
                purge_tenant,
                purge_connection,
                "purge-connector-call",
            ),
            (
                neighbour_binding,
                neighbor_tenant,
                neighbour_connection,
                "neighbour-connector-call",
            ),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_action_bindings \
                    (binding_uid, tenant_id, connection_uid, action_id, connection_generation, \
                     compiled_contract, contract_hash, governed_contract_revision, minimum_effect) \
                 VALUES ($1, $2, $3, 'read', 1, '{}'::JSONB, repeat('c', 64), \
                         'runtime-v1', 'allow')",
            )
            .bind(binding_uid)
            .bind(connection_tenant)
            .bind(connection_uid)
            .execute(&target)
            .await?;
            sqlx::query(
                "INSERT INTO moa.connector_action_invocations \
                    (invocation_uid, tenant_id, connection_uid, binding_uid, \
                     connection_generation, tool_call_id, request_hash) \
                 VALUES ($1, $2, $3, $4, 1, $5, repeat('d', 64))",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(connection_tenant)
            .bind(connection_uid)
            .bind(binding_uid)
            .bind(tool_call_id)
            .execute(&target)
            .await?;
        }
        sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
            .bind(purge_tenant)
            .bind(&purge_operation)
            .execute(&target)
            .await?;

        let subject_only_tenant = uuid::Uuid::new_v4();
        let committed_tenant = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO moa.destruction_operation_fence \
                (tenant_id, subject_id, operation_id, operation_kind) \
             VALUES ($1, $2, 'subject-only-probe', 'privacy.erase')",
        )
        .bind(subject_only_tenant)
        .bind(uuid::Uuid::new_v4())
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO moa.destruction_operation_fence \
                (tenant_id, subject_id, operation_id, operation_kind, status, committed_at) \
             VALUES ($1, NULL, 'committed-probe', 'privacy.erase', 'committed', now())",
        )
        .bind(committed_tenant)
        .execute(&target)
        .await?;
        let helper_scope_facts: (bool, bool, bool) = sqlx::query_as(
            "SELECT moa.tenant_write_fenced($1), \
                    moa.tenant_write_fenced($2), \
                    moa.tenant_write_fenced($3)",
        )
        .bind(purge_tenant)
        .bind(subject_only_tenant)
        .bind(committed_tenant)
        .fetch_one(&target)
        .await?;
        let mut activator = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_artifact_activator")
            .execute(&mut *activator)
            .await?;
        let activator_fenced: bool = sqlx::query_scalar("SELECT moa.tenant_write_fenced($1)")
            .bind(purge_tenant)
            .fetch_one(&mut *activator)
            .await?;
        activator.rollback().await?;
        let mut eraser = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_privacy_eraser")
            .execute(&mut *eraser)
            .await?;
        let eraser_fenced: bool = sqlx::query_scalar("SELECT moa.tenant_write_fenced($1)")
            .bind(purge_tenant)
            .fetch_one(&mut *eraser)
            .await?;
        eraser.rollback().await?;

        sqlx::raw_sql(
            "GRANT INSERT, SELECT ON authz_outbox TO moa_app; \
             GRANT SELECT ON moa.tenant_purge_operations TO moa_app; \
             GRANT SELECT ON moa.destruction_operation_fence TO moa_app;",
        )
        .execute(&target)
        .await?;
        let mut spoof = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *spoof)
            .await?;
        sqlx::query(
            "SELECT set_config('moa.tenant_id', $1, true), \
                    set_config('moa.tenant_purge_operation_id', $2, true)",
        )
        .bind(purge_tenant.to_string())
        .bind(&purge_operation)
        .execute(&mut *spoof)
        .await?;
        let spoof_error = sqlx::query(
            "INSERT INTO authz_outbox \
                (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id) \
             VALUES ('write', $1, 'operator', $2, 5, $3)",
        )
        .bind(format!("operator:{}", uuid::Uuid::new_v4()))
        .bind(format!("tenant:{purge_tenant}"))
        .bind(purge_tenant)
        .execute(&mut *spoof)
        .await
        .expect_err("a spoofed purge GUC must not authorize a desired write");
        let spoof_sqlstate = spoof_error
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()));
        spoof.rollback().await?;

        let first_authz: (i32, i32, bool) = sqlx::query_as(
            "SELECT scanned, inverted, exhausted \
             FROM moa.invert_tenant_authz_batch($1, $2)",
        )
        .bind(purge_tenant)
        .bind(&purge_operation)
        .fetch_one(&target)
        .await?;
        let second_authz: (i32, i32, bool) = sqlx::query_as(
            "SELECT scanned, inverted, exhausted \
             FROM moa.invert_tenant_authz_batch($1, $2)",
        )
        .bind(purge_tenant)
        .bind(&purge_operation)
        .fetch_one(&target)
        .await?;
        let final_authz: (i32, i32, bool) = sqlx::query_as(
            "SELECT scanned, inverted, exhausted \
             FROM moa.invert_tenant_authz_batch($1, $2)",
        )
        .bind(purge_tenant)
        .bind(&purge_operation)
        .fetch_one(&target)
        .await?;

        let mut terminal = None;
        let mut release_policy_batches = Vec::new();
        for _ in 0..300 {
            let batch: (String, String, i64) = sqlx::query_as(
                "SELECT batch_state, stage, affected \
                 FROM moa.run_tenant_purge_batch($1, $2)",
            )
            .bind(purge_tenant)
            .bind(&purge_operation)
            .fetch_one(&target)
            .await?;
            if batch.1 == "moa.artifact_release_policy" && batch.2 > 0 {
                release_policy_batches.push(batch.2);
            }
            if batch.0 == "committed" || batch.0 == "already_committed" {
                terminal = Some(batch);
                break;
            }
        }
        let bounded_facts: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT \
                (SELECT count(*) FROM users WHERE tenant_id = $1), \
                (SELECT count(*) FROM tenants WHERE id = $1), \
                (SELECT count(*) FROM tenants WHERE id = $2), \
                (SELECT count(*) FROM authz_outbox \
                 WHERE tenant_id = $1 AND op = 'delete' AND status = 'pending'), \
                (SELECT total_deleted_count FROM moa.tenant_purge_operations \
                 WHERE tenant_id = $1 AND status = 'relationally_committed'), \
                (SELECT count(*) FROM moa.artifact_release_policy \
                 WHERE storage_partition_id = $1::TEXT), \
                (SELECT count(*) FROM moa.artifact_release_policy \
                 WHERE storage_partition_id = $2::TEXT)",
        )
        .bind(purge_tenant)
        .bind(neighbor_tenant)
        .fetch_one(&target)
        .await?;
        let activation_chain_facts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT \
                (SELECT count(*) FROM moa.artifact_activation_audit \
                 WHERE storage_partition_id = $1::TEXT), \
                (SELECT count(*) FROM moa.artifact_activation_audit \
                 WHERE storage_partition_id = $2::TEXT), \
                (SELECT count(*) FROM moa.artifact_serving_pointer \
                 WHERE storage_partition_id = $1::TEXT), \
                (SELECT count(*) FROM moa.artifact_serving_pointer \
                 WHERE storage_partition_id = $2::TEXT), \
                (SELECT count(*) FROM moa.artifact \
                 WHERE storage_partition_id = $1::TEXT), \
                (SELECT count(*) FROM moa.artifact \
                 WHERE storage_partition_id = $2::TEXT)",
        )
        .bind(purge_tenant)
        .bind(neighbor_tenant)
        .fetch_one(&target)
        .await?;
        let connector_facts: (i64, i64, i64, i64, i64, i64, i64, i64) =
            sqlx::query_as(
            "SELECT \
                (SELECT count(*) FROM moa.connector_action_invocations WHERE tenant_id = $1), \
                (SELECT count(*) FROM moa.connector_action_bindings WHERE tenant_id = $1), \
                (SELECT count(*) FROM moa.connector_connection_use_grants WHERE tenant_id = $1), \
                (SELECT count(*) FROM moa.connector_connections WHERE tenant_id = $1), \
                (SELECT count(*) FROM moa.connector_action_invocations WHERE tenant_id = $2), \
                (SELECT count(*) FROM moa.connector_action_bindings WHERE tenant_id = $2), \
                (SELECT count(*) FROM moa.connector_connection_use_grants WHERE tenant_id = $2), \
                (SELECT count(*) FROM moa.connector_connections WHERE tenant_id = $2)",
            )
            .bind(purge_tenant)
            .bind(neighbor_tenant)
            .fetch_one(&target)
            .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            migrated,
            catalog_count,
            trigger_kinds,
            global_exemptions,
            fence_helper_contract,
            restricted_fence_select_grants,
            legacy_release_cleanup,
            audit_guard_contract,
            helper_scope_facts,
            activator_fenced,
            eraser_fenced,
            first_authz,
            second_authz,
            final_authz,
            release_policy_batches,
            terminal,
            bounded_facts,
            activation_chain_facts,
            connector_facts,
            spoof_sqlstate,
            ordinary_activation_delete_sqlstate,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;
    let (
        first,
        second,
        migrated,
        catalog_count,
        trigger_kinds,
        global_exemptions,
        fence_helper_contract,
        restricted_fence_select_grants,
        legacy_release_cleanup,
        audit_guard_contract,
        helper_scope_facts,
        activator_fenced,
        eraser_fenced,
        first_authz,
        second_authz,
        final_authz,
        release_policy_batches,
        terminal,
        bounded_facts,
        activation_chain_facts,
        connector_facts,
        spoof_sqlstate,
        ordinary_activation_delete_sqlstate,
    ) = outcome.expect("bounded tenant-purge assertions should complete");
    assert_eq!(first, expected_migration_labels());
    assert!(
        second.is_empty(),
        "the final schema must not reapply: {second:?}"
    );
    assert_eq!(
        migrated,
        (
            "in_progress".to_string(),
            "authz".to_string(),
            0,
            0,
            0,
            true,
            true,
        )
    );
    assert_eq!(catalog_count, 133);
    assert_eq!(
        trigger_kinds,
        vec![
            "moa_tenant_purge_fence_insert".to_string(),
            "moa_tenant_purge_fence_update".to_string(),
        ]
    );
    assert_eq!(global_exemptions, 0);
    assert_eq!(
        fence_helper_contract,
        (
            "moa_owner".to_string(),
            true,
            vec!["search_path=pg_catalog, pg_temp".to_string()],
            true,
            true,
            true,
        ),
        "the fence helper must remain owner-defined and least privilege while the statement guard remains invoker-rights"
    );
    assert_eq!(
        restricted_fence_select_grants, 0,
        "restricted definer roles must not read tenant purge control tables directly"
    );
    assert!(
        legacy_release_cleanup.0,
        "the legacy monolithic release purge function must be absent"
    );
    assert_eq!(
        legacy_release_cleanup.1, 0,
        "all legacy release read/delete policies must be absent"
    );
    assert_eq!(
        legacy_release_cleanup.2, 0,
        "the inert releaser role must retain no release-table privileges"
    );
    assert!(
        !legacy_release_cleanup.3,
        "the inert releaser role must retain no moa schema usage"
    );
    assert!(
        legacy_release_cleanup
            .4
            .contains("moa.tenant_purge_bypass_valid")
            && !legacy_release_cleanup.4.contains("moa_artifact_releaser")
            && !legacy_release_cleanup
                .4
                .contains("artifact_release_purge_partition"),
        "the audit guard must admit deletion only through the validated bounded purge: {}",
        legacy_release_cleanup.4
    );
    assert_eq!(
        audit_guard_contract,
        (
            "moa_owner".to_string(),
            true,
            vec!["search_path=pg_catalog, pg_temp".to_string()],
            true,
            true,
        ),
        "the activation-audit purge exception must run under a hardened owner-only trigger function with no direct non-owner execution"
    );
    assert_eq!(
        helper_scope_facts,
        (true, false, false),
        "only an in-progress tenant-wide fence may trip the restricted-writer helper"
    );
    assert!(
        activator_fenced,
        "artifact activator must execute the helper"
    );
    assert!(eraser_fenced, "privacy eraser must execute the helper");
    assert_eq!(first_authz, (1000, 1000, false));
    assert_eq!(second_authz, (1, 1, false));
    assert_eq!(final_authz, (0, 0, true));
    assert_eq!(
        release_policy_batches,
        vec![1000, 1],
        "the release-policy stage must cross the fixed 1,000-row boundary"
    );
    assert_eq!(
        terminal,
        Some(("committed".to_string(), "complete".to_string(), 0))
    );
    assert_eq!(
        bounded_facts,
        (0, 0, 1, 1001, 2012, 0, 1),
        "the target release-policy set must be gone while the neighboring policy survives"
    );
    assert_eq!(
        activation_chain_facts,
        (0, 1, 0, 1, 0, 1),
        "the target activation chain must be gone while the neighboring chain survives"
    );
    assert_eq!(
        connector_facts,
        (0, 0, 0, 0, 1, 1, 1, 1),
        "connector purge must follow invocation -> binding -> direct-use grant -> connection order without touching the neighboring tenant"
    );
    assert_eq!(spoof_sqlstate.as_deref(), Some("55000"));
    assert_eq!(
        ordinary_activation_delete_sqlstate.as_deref(),
        Some("P0001"),
        "ordinary callers must not bypass the activation audit's append-only guard"
    );
}
