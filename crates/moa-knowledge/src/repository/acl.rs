//! Postgres persistence for provider source ACL snapshots and principal bindings.
//!
//! Every write here is ordered the same way: take the governing object's row
//! lock first, then insert the immutable snapshot and its entries in canonical
//! fingerprint order, then move the object's current pointer. Concurrent syncs
//! of the same object therefore queue on one row instead of interleaving their
//! entry writes, which is the shape that produced the `40P01` deadlock class in
//! graph ingestion before uid-ordered writes were introduced.

use super::row_mapping::*;
use super::*;

/// Inserts one immutable snapshot with its entries and makes it current.
///
/// The whole replacement is one transaction: an object never observes a snapshot
/// whose entries are partially written, and it never points at a snapshot that
/// does not exist. Re-capturing byte-identical permissions is idempotent through
/// the `(tenant, object, revision, hash)` unique index, which is what lets a
/// resync converge without minting a new snapshot on every pass.
pub(super) async fn replace_object_acl_snapshot(
    repository: &PostgresKnowledgeRepository,
    snapshot: ProviderAclSnapshot,
) -> Result<ProviderAclSnapshot> {
    let mut conn = repository.begin().await?;

    // Lock the governing object first so two syncs of the same object serialize
    // here rather than on the snapshot rows they are each inserting.
    let locked = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT object_uid
        FROM moa.knowledge_objects
        WHERE object_uid = $1 AND tenant_id = $2
        FOR UPDATE
        "#,
    )
    .bind(snapshot.object_uid)
    .bind(snapshot.tenant_id.0)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    if locked.is_none() {
        return Err(Error::Repository(
            "knowledge object was not visible for an ACL snapshot replacement".to_string(),
        ));
    }

    let existing = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT snapshot_uid
        FROM moa.knowledge_source_acl_snapshots
        WHERE tenant_id = $1
          AND object_id = $2
          AND provider_revision = $3
          AND snapshot_hash = $4
        "#,
    )
    .bind(snapshot.tenant_id.0)
    .bind(snapshot.object_uid)
    .bind(&snapshot.provider_revision)
    .bind(&snapshot.snapshot_hash)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    let snapshot_uid = match existing {
        Some(snapshot_uid) => snapshot_uid,
        None => {
            sqlx::query(
                r#"
                INSERT INTO moa.knowledge_source_acl_snapshots (
                    snapshot_uid, tenant_id, storage_partition_id, connection_id, object_id,
                    provider_revision, snapshot_hash, provenance, complete, entry_count, captured_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                "#,
            )
            .bind(snapshot.snapshot_uid)
            .bind(snapshot.tenant_id.0)
            .bind(storage_partition_id(snapshot.tenant_id))
            .bind(snapshot.connection_uid)
            .bind(snapshot.object_uid)
            .bind(&snapshot.provider_revision)
            .bind(&snapshot.snapshot_hash)
            .bind(snapshot.provenance.as_str())
            .bind(snapshot.complete)
            .bind(i32::try_from(snapshot.entries.len()).map_err(map_int_error)?)
            .bind(snapshot.captured_at)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;

            // Entries arrive canonically sorted from `ProviderAclSnapshot::normalized`
            // and are inserted in that order, so two writers racing on the same
            // snapshot acquire row locks in the same sequence.
            for (index, entry) in snapshot.entries.iter().enumerate() {
                sqlx::query(
                    r#"
                    INSERT INTO moa.knowledge_source_acl_entries (
                        entry_uid, tenant_id, storage_partition_id, snapshot_id,
                        entry_kind, principal_kind, principal_fingerprint, fingerprint_key_version
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                    ON CONFLICT (snapshot_id, entry_kind, principal_fingerprint) DO NOTHING
                    "#,
                )
                .bind(entry_uid(snapshot.snapshot_uid, index))
                .bind(snapshot.tenant_id.0)
                .bind(storage_partition_id(snapshot.tenant_id))
                .bind(snapshot.snapshot_uid)
                .bind(entry.entry_kind.as_str())
                .bind(entry.principal_kind.as_str())
                .bind(entry.principal.as_bytes())
                .bind(i32::from(entry.principal.key_version()))
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?;
            }
            snapshot.snapshot_uid
        }
    };

    // An incomplete capture never becomes the current snapshot: it is recorded
    // as evidence, and the object stays hidden. Recording it still bumps the
    // epoch, so a warm cache built while the ACL looked complete is dropped.
    let next_state = if snapshot.complete {
        SourceAclState::Current
    } else {
        SourceAclState::Incomplete
    };
    let current_snapshot_id = snapshot.complete.then_some(snapshot_uid);
    let acl_revision = snapshot
        .complete
        .then(|| snapshot.provider_revision.clone());

    sqlx::query(
        r#"
        UPDATE moa.knowledge_objects
        SET acl_state = $3,
            acl_revision = $4,
            current_acl_snapshot_id = $5,
            updated_at = now()
        WHERE object_uid = $1 AND tenant_id = $2
        "#,
    )
    .bind(snapshot.object_uid)
    .bind(snapshot.tenant_id.0)
    .bind(next_state.as_str())
    .bind(acl_revision)
    .bind(current_snapshot_id)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    conn.commit().await.map_err(map_moa_error)?;
    Ok(ProviderAclSnapshot {
        snapshot_uid,
        ..snapshot
    })
}

/// Marks one object's ACL stale because the provider announced a newer revision.
///
/// Applied before MOA has captured the new permissions, so the object stops
/// being retrievable immediately instead of serving the old ACL until a resync
/// finishes. The current snapshot pointer is cleared for the same reason: a
/// pointer that outlives its validity is exactly the ambiguity this design
/// refuses.
pub(super) async fn mark_object_acl_stale(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
    announced_revision: Option<&str>,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    let result = sqlx::query(
        r#"
        UPDATE moa.knowledge_objects
        SET acl_state = 'stale',
            acl_revision = COALESCE($2, acl_revision),
            current_acl_snapshot_id = NULL,
            updated_at = now()
        WHERE object_uid = $1
          AND tenant_id = $3
        "#,
    )
    .bind(object_uid)
    .bind(announced_revision)
    .bind(repository.scoped_tenant_id().0)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    ensure_rows_affected(result.rows_affected(), "mark knowledge object ACL stale")
}

/// Reads one object's stored ACL position.
pub(super) async fn object_acl(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
) -> Result<Option<ObjectAcl>> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        SELECT acl_state, acl_revision, current_acl_snapshot_id
        FROM moa.knowledge_objects
        WHERE object_uid = $1 AND tenant_id = $2
        "#,
    )
    .bind(object_uid)
    .bind(repository.scoped_tenant_id().0)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.map(|row| {
        Ok(ObjectAcl {
            state: SourceAclState::parse(row.try_get("acl_state").map_err(map_sqlx_error)?)?,
            revision: row.try_get("acl_revision").map_err(map_sqlx_error)?,
            current_snapshot_uid: row
                .try_get("current_acl_snapshot_id")
                .map_err(map_sqlx_error)?,
        })
    })
    .transpose()
}

/// Loads the entries of one stored snapshot for inspection and offline checks.
pub(super) async fn snapshot_entries(
    repository: &PostgresKnowledgeRepository,
    snapshot_uid: Uuid,
) -> Result<Vec<ProviderAclEntry>> {
    let mut conn = repository.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT entry_kind, principal_kind, principal_fingerprint
        FROM moa.knowledge_source_acl_entries
        WHERE snapshot_id = $1 AND tenant_id = $2
        ORDER BY entry_kind, principal_fingerprint
        "#,
    )
    .bind(snapshot_uid)
    .bind(repository.scoped_tenant_id().0)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter()
        .map(|row| {
            Ok(ProviderAclEntry {
                entry_kind: SourceAclEntryKind::parse(
                    row.try_get("entry_kind").map_err(map_sqlx_error)?,
                )?,
                principal_kind: SourcePrincipalKind::parse(
                    row.try_get("principal_kind").map_err(map_sqlx_error)?,
                )?,
                principal: SourcePrincipalFingerprint::from_bytes(
                    row.try_get::<Vec<u8>, _>("principal_fingerprint")
                        .map_err(map_sqlx_error)?
                        .as_slice(),
                )
                .map_err(map_moa_error)?,
            })
        })
        .collect()
}

/// Binds one verified provider principal to a contact, or to the whole tenant.
///
/// `contact_id` of [`moa_db::TENANT_WIDE_PRINCIPAL_HOLDER`] means every member of
/// the tenant holds this principal, which is how a provider's "anyone with
/// access" grant is represented without fanning a row out per contact.
pub(super) async fn upsert_principal_binding(
    repository: &PostgresKnowledgeRepository,
    binding: SourcePrincipalBinding,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_source_principal_bindings (
            binding_uid, tenant_id, storage_partition_id, contact_id, connection_id,
            principal_kind, principal_fingerprint, fingerprint_key_version, verified_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (
            tenant_id,
            contact_id,
            principal_fingerprint,
            COALESCE(connection_id, '00000000-0000-0000-0000-000000000000'::UUID)
        )
        DO UPDATE SET
            principal_kind = EXCLUDED.principal_kind,
            fingerprint_key_version = EXCLUDED.fingerprint_key_version,
            verified_at = EXCLUDED.verified_at,
            updated_at = now()
        "#,
    )
    .bind(binding.binding_uid)
    .bind(binding.tenant_id.0)
    .bind(storage_partition_id(binding.tenant_id))
    .bind(binding.contact_id)
    .bind(binding.connection_uid)
    .bind(binding.principal_kind.as_str())
    .bind(binding.principal.as_bytes())
    .bind(i32::from(binding.principal.key_version()))
    .bind(binding.verified_at)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    Ok(())
}

/// Records that holders of `member` also hold `group`.
///
/// Retrieval expands exactly one level over these edges, so an adapter that
/// discovers nested groups must flatten them here rather than relying on a
/// recursive read on the request path.
pub(super) async fn upsert_group_binding(
    repository: &PostgresKnowledgeRepository,
    binding: SourcePrincipalGroupBinding,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_source_principal_group_bindings (
            binding_uid, tenant_id, storage_partition_id, connection_id,
            member_fingerprint, group_kind, group_fingerprint, fingerprint_key_version, verified_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (
            tenant_id,
            member_fingerprint,
            group_fingerprint,
            COALESCE(connection_id, '00000000-0000-0000-0000-000000000000'::UUID)
        )
        DO UPDATE SET
            group_kind = EXCLUDED.group_kind,
            fingerprint_key_version = EXCLUDED.fingerprint_key_version,
            verified_at = EXCLUDED.verified_at,
            updated_at = now()
        "#,
    )
    .bind(binding.binding_uid)
    .bind(binding.tenant_id.0)
    .bind(storage_partition_id(binding.tenant_id))
    .bind(binding.connection_uid)
    .bind(binding.member.as_bytes())
    .bind(binding.group_kind.as_str())
    .bind(binding.group.as_bytes())
    .bind(i32::from(binding.group.key_version()))
    .bind(binding.verified_at)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    Ok(())
}

/// Removes every principal and group binding for one contact.
///
/// Called on offboarding: the contact keeps existing, but stops holding any
/// provider principal, so provider-managed content stops being admitted for them
/// on the next request rather than at the next sync.
pub(super) async fn revoke_contact_principals(
    repository: &PostgresKnowledgeRepository,
    contact_id: Uuid,
) -> Result<u64> {
    let mut conn = repository.begin().await?;
    let removed = sqlx::query(
        r#"
        DELETE FROM moa.knowledge_source_principal_bindings
        WHERE tenant_id = $1 AND contact_id = $2
        "#,
    )
    .bind(repository.scoped_tenant_id().0)
    .bind(contact_id)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    Ok(removed.rows_affected())
}

/// Derives a deterministic entry identifier from its snapshot and position.
///
/// Deterministic so replaying an interrupted snapshot insert produces the same
/// rows rather than duplicates under a fresh random id.
fn entry_uid(snapshot_uid: Uuid, index: usize) -> Uuid {
    crate::graph_delta::stable_uid(&format!("source-acl-entry:{snapshot_uid}:{index}"))
}
