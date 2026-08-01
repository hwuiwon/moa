//! Artifact revision and package-file persistence.

use super::*;
use crate::release::{ActivationTargetClass, TenantScope};
use crate::validation::validate_for_status;
use moa_core::types::contact::ContactId;

/// Outcome of attempting to roll a serving skill revision back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackApplication {
    /// The regressed revision was serving; the serving pointer was removed and
    /// the revision archived. The artifact now serves nothing.
    Applied,
    /// A different revision serves now, so nothing was changed. The rollback
    /// proposal is stale and must not be retried.
    Superseded {
        /// Revision the serving pointer names.
        serving_revision_uid: Uuid,
    },
    /// Nothing was serving for this artifact, so there was nothing to roll back.
    NotServing,
}

/// Returns the activation target label a serving pointer of this kind carries.
fn pointer_activation_target(kind: &ArtifactKind) -> Result<&'static str> {
    ActivationTargetClass::for_artifact_kind(kind)
        .map(|class| class.as_str())
        .ok_or_else(|| {
            MoaError::StorageError(format!(
                "artifact kind {kind} has no release-gated serving pointer"
            ))
        })
}

impl ArtifactRegistry {
    /// Creates a new draft revision and stores optional package files.
    pub async fn create_draft(
        &self,
        scope: &ActionRuleScope,
        draft: NewArtifactDraft<'_>,
    ) -> Result<StoredArtifactRevision> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let stored = Self::create_draft_in_tx(conn.as_mut(), scope, draft).await?;
        conn.commit().await?;
        Ok(stored)
    }

    /// Creates a new draft revision using the caller's open transaction.
    ///
    /// The caller owns commit or rollback and should apply matching MOA scope GUCs before calling
    /// this method when row-level security is relevant.
    pub async fn create_draft_in_tx(
        conn: &mut PgConnection,
        scope: &ActionRuleScope,
        draft: NewArtifactDraft<'_>,
    ) -> Result<StoredArtifactRevision> {
        validate_source_format(draft.source_format)?;
        // A contact-scoped skill, action, or agent has no representable release
        // subject, so it could never be evaluated and could never serve. Refusing
        // it here is what makes contact-scoped release subjects unrepresentable
        // rather than merely unactivatable: migration V000373 archived the ones
        // that already existed, and this is why none can come back.
        if ActivationTargetClass::is_release_gated(&draft.document.kind)
            && matches!(scope, ActionRuleScope::Contact { .. })
        {
            return Err(MoaError::ValidationError(format!(
                "artifact kind {} cannot be contact-scoped; release subjects accept a tenant \
                 scope only",
                draft.document.kind
            )));
        }
        let parts = ArtifactScopeParts::from_scope(scope);
        let artifact_uid = ensure_artifact(conn, &parts, draft.document).await?;
        let version = next_revision_version(conn, artifact_uid).await?;
        let revision_uid = Uuid::now_v7();
        let definition = serde_json::to_value(draft.document)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        let canonical_hash = canonical_hash(draft.document)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?
            .to_vec();
        let validation_report =
            serde_json::to_value(validate_for_status(draft.document, ArtifactStatus::Draft))
                .map_err(|error| MoaError::SerializationError(error.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO moa.artifact_revision (
                revision_uid, artifact_uid, tenant_id, storage_partition_id, user_id, definition,
                canonical_hash, source_format, source_text, status,
                validation_report, version
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'draft', $10, $11)
            "#,
        )
        .bind(revision_uid)
        .bind(artifact_uid)
        .bind(parts.tenant_id)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(definition)
        .bind(&canonical_hash)
        .bind(draft.source_format)
        .bind(draft.source_text)
        .bind(validation_report)
        .bind(version)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        sqlx::query(
            "UPDATE moa.artifact SET latest_revision_uid = $1, updated_at = now() WHERE artifact_uid = $2",
        )
        .bind(revision_uid)
        .bind(artifact_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        insert_files(conn, &parts, artifact_uid, revision_uid, draft.files).await?;
        load_revision_by_uid(conn, revision_uid).await
    }

    /// Validates a draft revision of a kind whose activation seam is owned elsewhere.
    ///
    /// This is deliberately not a generic publish helper. Release-gated kinds
    /// (skill, action, agent) are refused: their serving pointer moves only
    /// through [`crate::registry::ReleaseRepository::activate`], and a helper that
    /// could mark them published would be exactly the bypass that made
    /// `Artifacts/publish`-only hooks pointless. What remains is the connector and
    /// experiment-plan path, where "published" means validated configuration and
    /// no session-visible serving transition happens here.
    pub async fn publish_unserved_revision(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
        report: &ValidationReport,
    ) -> Result<StoredArtifactRevision> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let stored =
            Self::publish_unserved_revision_in_tx(conn.as_mut(), revision_uid, report).await?;
        conn.commit().await?;
        Ok(stored)
    }

    /// Validates an unserved revision using the caller's open transaction.
    ///
    /// The caller owns commit or rollback and should apply matching MOA scope GUCs before calling
    /// this method when row-level security is relevant.
    pub async fn publish_unserved_revision_in_tx(
        conn: &mut PgConnection,
        revision_uid: Uuid,
        report: &ValidationReport,
    ) -> Result<StoredArtifactRevision> {
        let row = sqlx::query(
            r#"
            SELECT r.artifact_uid, a.kind
            FROM moa.artifact_revision r
            JOIN moa.artifact a ON a.artifact_uid = r.artifact_uid
            WHERE r.revision_uid = $1
              AND r.valid_to IS NULL
            FOR UPDATE OF r
            "#,
        )
        .bind(revision_uid)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let artifact_uid: Uuid = row.try_get("artifact_uid").map_err(map_sqlx_error)?;
        let kind_text: String = row.try_get("kind").map_err(map_sqlx_error)?;
        let kind: ArtifactKind = kind_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?;
        if ActivationTargetClass::is_release_gated(&kind) {
            return Err(MoaError::ValidationError(format!(
                "artifact kind {kind} is release-gated; revision {revision_uid} can only serve \
                 through an attested activation"
            )));
        }

        let validation_report = serde_json::to_value(report)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        sqlx::query(
            r#"
            UPDATE moa.artifact_revision
            SET status = 'published',
                validation_report = $2,
                published_at = COALESCE(published_at, now()),
                updated_at = now()
            WHERE revision_uid = $1
              AND valid_to IS NULL
        "#,
        )
        .bind(revision_uid)
        .bind(validation_report)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        sqlx::query(
            "UPDATE moa.artifact SET latest_revision_uid = $1, updated_at = now() WHERE artifact_uid = $2",
        )
        .bind(revision_uid)
        .bind(artifact_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        load_revision_by_uid(conn, revision_uid).await
    }

    /// Rolls a serving skill revision back by un-serving it.
    ///
    /// A rollback stops a regressed revision from serving. It does not promote a
    /// predecessor: restoring an older revision is itself a serving transition,
    /// and every serving transition fails closed through an attested activation.
    /// So this deletes the serving pointer, archives the regressed revision, and
    /// drops the stale identity embedding so nearest-neighbour consumers stop
    /// advertising it. A skill whose regressed revision is the only one that ever
    /// served has its artifact identity retired as well, since nothing is left to
    /// advertise; when an earlier revision did serve, the identity survives so that
    /// revision can be re-released through the gate.
    ///
    /// The pointer row is read `FOR UPDATE`, so a concurrent activation either
    /// happens entirely before this rollback or fails its compare-and-set.
    /// A proposal filed against a revision that no longer serves changes nothing
    /// and reports [`RollbackApplication::Superseded`]; the caller must treat that
    /// as terminal rather than retry. The caller owns commit or rollback and
    /// should apply matching MOA scope GUCs before calling.
    pub async fn rollback_serving_revision_in_tx(
        conn: &mut PgConnection,
        scope: &TenantScope,
        promoted_revision_uid: Uuid,
        expected_activation_audit_uid: Uuid,
        expected_pointer_version: i64,
        actor: &str,
        reason: Option<&str>,
    ) -> Result<RollbackApplication> {
        let artifact_uid = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT artifact_uid
            FROM moa.artifact_revision
            WHERE revision_uid = $1
              AND valid_to IS NULL
            FOR UPDATE
            "#,
        )
        .bind(promoted_revision_uid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            MoaError::ValidationError("promoted artifact revision no longer exists".to_string())
        })?;

        super::release::lock_artifact_serving_pointer(conn, scope, artifact_uid)
            .await
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
        let Some(pointer) =
            super::serving::load_serving_pointer_in_tx(conn, scope, artifact_uid, false).await?
        else {
            return Ok(RollbackApplication::NotServing);
        };
        if pointer.revision_uid != promoted_revision_uid {
            return Ok(RollbackApplication::Superseded {
                serving_revision_uid: pointer.revision_uid,
            });
        }
        let removed: i64 = sqlx::query_scalar(
            r#"
            SELECT moa.apply_artifact_rollback_transition(
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(scope.storage_partition_id().to_string())
        .bind(artifact_uid)
        .bind(pointer_activation_target(&pointer.kind)?)
        .bind(expected_activation_audit_uid)
        .bind(promoted_revision_uid)
        .bind(expected_pointer_version)
        .bind(expected_pointer_version.saturating_add(1))
        .bind(actor)
        .bind(reason)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        if removed != 1 {
            return Ok(RollbackApplication::Superseded {
                serving_revision_uid: pointer.revision_uid,
            });
        }

        sqlx::query(
            r#"
            UPDATE moa.artifact_revision
            SET status = 'archived',
                updated_at = now()
            WHERE revision_uid = $1
              AND valid_to IS NULL
            "#,
        )
        .bind(promoted_revision_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        sqlx::query(
            r#"
            UPDATE moa.artifact_release_candidate
            SET slot = 'released',
                updated_at = now()
            WHERE revision_uid = $1
            "#,
        )
        .bind(promoted_revision_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        super::skill_embeddings::delete_skill_embedding_in_tx(conn, artifact_uid).await?;

        // A created skill -- one no other revision ever served for -- has nothing
        // left to advertise, so its identity is retired rather than left as an empty
        // shell that listings and rankings still enumerate. When an earlier revision
        // did serve, the identity survives so that revision can be re-released.
        let served_before = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM moa.artifact_activation_audit
                WHERE artifact_uid = $1
                  AND decision_kind = 'activation'
                  AND activated_revision_uid IS DISTINCT FROM $2
            )
            "#,
        )
        .bind(artifact_uid)
        .bind(promoted_revision_uid)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        if !served_before {
            sqlx::query(
                r#"
                UPDATE moa.artifact
                SET valid_to = now(), updated_at = now()
                WHERE artifact_uid = $1
                  AND valid_to IS NULL
                "#,
            )
            .bind(artifact_uid)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        }
        Ok(RollbackApplication::Applied)
    }

    /// Loads the most specific visible artifact revision by kind and name.
    pub async fn load_visible(
        &self,
        scope: &ActionRuleScope,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<Option<StoredArtifactRevision>> {
        load_visible_with_status(&self.pool, scope, kind, name, None).await
    }

    /// Loads the most specific visible `published` artifact revision by kind and name.
    ///
    /// Only kinds whose activation seam is owned elsewhere reach `published` --
    /// connector catalogs and experiment plans -- so this is not a serving lookup
    /// for release-gated kinds. Skill, action, and agent resolution goes through
    /// [`Self::load_serving`] and the agent installation pointer instead.
    pub async fn load_visible_published(
        &self,
        scope: &ActionRuleScope,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<Option<StoredArtifactRevision>> {
        load_visible_with_status(
            &self.pool,
            scope,
            kind,
            name,
            Some(ArtifactStatus::Published),
        )
        .await
    }

    /// Loads a visible artifact revision by revision id.
    pub async fn load_revision(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
    ) -> Result<Option<StoredArtifactRevision>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(&format!(
            r#"
            SELECT {REVISION_COLUMNS}
            FROM moa.artifact a
            JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
            WHERE a.valid_to IS NULL
              AND r.revision_uid = $3
              AND r.valid_to IS NULL
              AND a.storage_partition_id = $1
              AND (a.user_id IS NULL OR a.user_id = $2)
            LIMIT 1
            "#
        ))
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(revision_uid)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(revision_from_row).transpose()
    }

    /// Stores a validation report against a revision without changing its status.
    ///
    /// Generic validation makes an immutable revision eligible for evaluation. It
    /// does not make it visible: a release-gated revision stays a draft here, and
    /// only an attested activation can move its serving pointer.
    pub async fn record_validation_report(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
        report: &ValidationReport,
    ) -> Result<StoredArtifactRevision> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let stored =
            Self::record_validation_report_in_tx(conn.as_mut(), revision_uid, report).await?;
        conn.commit().await?;
        Ok(stored)
    }

    /// Stores a validation report using the caller's open transaction.
    ///
    /// The caller owns commit or rollback and should apply matching MOA scope GUCs
    /// before calling. Used by the skill-learning review, whose regression run is
    /// the eligibility step for a distilled candidate.
    pub async fn record_validation_report_in_tx(
        conn: &mut PgConnection,
        revision_uid: Uuid,
        report: &ValidationReport,
    ) -> Result<StoredArtifactRevision> {
        let validation_report = serde_json::to_value(report)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        let updated = sqlx::query(
            r#"
            UPDATE moa.artifact_revision
            SET validation_report = $2,
                updated_at = now()
            WHERE revision_uid = $1
              AND valid_to IS NULL
            "#,
        )
        .bind(revision_uid)
        .bind(validation_report)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if updated != 1 {
            return Err(MoaError::ValidationError(format!(
                "artifact revision {revision_uid} does not exist or was invalidated"
            )));
        }
        load_revision_by_uid(conn, revision_uid).await
    }

    /// Loads one artifact revision by id using the caller's open transaction.
    ///
    /// Scope filtering is the caller's responsibility here: the caller already
    /// opened the transaction with the matching MOA scope GUCs, so row-level
    /// security is what bounds the read.
    pub async fn load_revision_in_tx(
        conn: &mut PgConnection,
        revision_uid: Uuid,
    ) -> Result<StoredArtifactRevision> {
        load_revision_by_uid(conn, revision_uid).await
    }

    /// Lists active artifact revisions visible from the provided scope.
    pub async fn list_visible(
        &self,
        scope: &ActionRuleScope,
        kind: Option<ArtifactKind>,
        status: Option<ArtifactStatus>,
    ) -> Result<Vec<ArtifactSummary>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT ON (a.kind, a.name)
                   a.artifact_uid, r.revision_uid, a.scope, a.kind, a.name,
                   a.description, a.tags, r.status, r.version, r.updated_at
            FROM moa.artifact a
            JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
            WHERE a.valid_to IS NULL
              AND r.valid_to IS NULL
              AND ($3::TEXT IS NULL OR a.kind = $3)
              AND ($4::TEXT IS NULL OR r.status = $4)
              AND a.storage_partition_id = $1
              AND (a.user_id IS NULL OR a.user_id IS NOT DISTINCT FROM $2)
            ORDER BY
              a.kind ASC,
              a.name ASC,
              CASE WHEN a.user_id IS NOT NULL THEN 0 ELSE 1 END ASC,
              r.version DESC
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(kind.as_ref().map(ToString::to_string))
        .bind(status.as_ref().map(ToString::to_string))
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(summary_from_row).collect()
    }

    /// Loads files attached to a visible revision.
    pub async fn load_files(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
    ) -> Result<Vec<ArtifactFile>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let files = load_files(conn.as_mut(), scope, revision_uid).await?;
        conn.commit().await?;
        Ok(files)
    }
}

async fn load_visible_with_status(
    pool: &PgPool,
    scope: &ActionRuleScope,
    kind: ArtifactKind,
    name: &str,
    status: Option<ArtifactStatus>,
) -> Result<Option<StoredArtifactRevision>> {
    let mut conn = ScopedConn::begin(pool, &artifact_scope_context(scope)).await?;
    let parts = ArtifactScopeParts::from_scope(scope);
    let row = sqlx::query(&format!(
        r#"
        SELECT {REVISION_COLUMNS}
        FROM moa.artifact a
        JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
        WHERE a.valid_to IS NULL
          AND r.valid_to IS NULL
          AND a.kind = $3
          AND a.name = $4
          AND ($5::TEXT IS NULL OR r.status = $5)
          AND a.storage_partition_id = $1
          AND (a.user_id IS NULL OR a.user_id IS NOT DISTINCT FROM $2)
        ORDER BY
          CASE WHEN a.user_id IS NOT NULL THEN 0 ELSE 1 END ASC,
          r.version DESC
        LIMIT 1
        "#
    ))
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(kind.to_string())
    .bind(name)
    .bind(status.as_ref().map(ToString::to_string))
    .fetch_optional(&mut *conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await?;
    row.as_ref().map(revision_from_row).transpose()
}

async fn ensure_artifact(
    conn: &mut PgConnection,
    parts: &ArtifactScopeParts,
    document: &ArtifactDocument,
) -> Result<Uuid> {
    let active = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT artifact_uid
        FROM moa.artifact
        WHERE valid_to IS NULL
          AND storage_partition_id IS NOT DISTINCT FROM $1
          AND user_id IS NOT DISTINCT FROM $2
          AND kind = $3
          AND name = $4
        FOR UPDATE
        "#,
    )
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(document.kind.to_string())
    .bind(&document.metadata.name)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    if let Some(artifact_uid) = active {
        sqlx::query(
            r#"
            UPDATE moa.artifact
            SET description = $2, tags = $3, updated_at = now()
            WHERE artifact_uid = $1
            "#,
        )
        .bind(artifact_uid)
        .bind(&document.metadata.description)
        .bind(&document.metadata.tags)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        return Ok(artifact_uid);
    }

    let artifact_uid = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.artifact (
            artifact_uid, tenant_id, storage_partition_id, user_id, kind, name, description, tags
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(artifact_uid)
    .bind(parts.tenant_id)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(document.kind.to_string())
    .bind(&document.metadata.name)
    .bind(&document.metadata.description)
    .bind(&document.metadata.tags)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    Ok(artifact_uid)
}

async fn next_revision_version(conn: &mut PgConnection, artifact_uid: Uuid) -> Result<i32> {
    let version = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT max(version) FROM moa.artifact_revision WHERE artifact_uid = $1",
    )
    .bind(artifact_uid)
    .fetch_one(&mut *conn)
    .await
    .map_err(map_sqlx_error)?
    .unwrap_or(0)
    .saturating_add(1);
    Ok(version)
}

async fn insert_files(
    conn: &mut PgConnection,
    parts: &ArtifactScopeParts,
    artifact_uid: Uuid,
    revision_uid: Uuid,
    files: &[NewArtifactFile],
) -> Result<()> {
    for file in files {
        if file.content.len() > MAX_FILE_SIZE_BYTES {
            return Err(MoaError::ValidationError(format!(
                "artifact file {} is too large: {} bytes exceeds the {MAX_FILE_SIZE_BYTES}-byte limit",
                file.path,
                file.content.len(),
            )));
        }
        let digest = Sha256::digest(&file.content).to_vec();
        let file_size_bytes = i64::try_from(file.content.len()).map_err(|_| {
            MoaError::ValidationError(format!("artifact file {} is too large", file.path))
        })?;
        sqlx::query(
            r#"
            INSERT INTO moa.artifact_file (
                file_uid, artifact_uid, revision_uid, tenant_id, storage_partition_id, user_id,
                path, content, content_sha256, content_type, executable,
                file_size_bytes
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(artifact_uid)
        .bind(revision_uid)
        .bind(parts.tenant_id)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(&file.path)
        .bind(&file.content)
        .bind(digest)
        .bind(file.content_type.as_deref())
        .bind(file.executable)
        .bind(file_size_bytes)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    }
    Ok(())
}

async fn load_revision_by_uid(
    conn: &mut PgConnection,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision> {
    let row = sqlx::query(&format!(
        r#"
        SELECT {REVISION_COLUMNS}
        FROM moa.artifact a
        JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
        WHERE r.revision_uid = $1
        "#
    ))
    .bind(revision_uid)
    .fetch_one(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;
    revision_from_row(&row)
}

async fn load_files(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<Vec<ArtifactFile>> {
    let parts = ArtifactScopeParts::from_scope(scope);
    let rows = sqlx::query(
        r#"
        SELECT f.file_uid, f.path, f.content, f.content_sha256, f.content_type,
               f.executable, f.file_size_bytes
        FROM moa.artifact_file f
        JOIN moa.artifact a ON a.artifact_uid = f.artifact_uid
        WHERE f.revision_uid = $3
          AND a.storage_partition_id = $1
          AND (a.user_id IS NULL OR a.user_id IS NOT DISTINCT FROM $2)
        ORDER BY f.path ASC
        "#,
    )
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(revision_uid)
    .fetch_all(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    rows.iter().map(file_from_row).collect()
}

fn scope_from_columns(
    scope: String,
    storage_partition_id: Option<String>,
    user_id: Option<String>,
) -> Result<(Option<StoragePartitionId>, Option<UserId>, String)> {
    let storage_partition_id = storage_partition_id.map(StoragePartitionId::new);
    let user_id = match (scope.as_str(), user_id) {
        ("tenant", None) => None,
        ("contact", Some(user_id)) => {
            parse_contact_user_id(&user_id)?;
            Some(UserId::new(user_id))
        }
        _ => {
            return Err(MoaError::StorageError(format!(
                "invalid artifact scope columns for `{scope}`"
            )));
        }
    };
    Ok((storage_partition_id, user_id, scope))
}

fn parse_contact_user_id(value: &str) -> Result<ContactId> {
    uuid::Uuid::parse_str(value)
        .map(ContactId)
        .map_err(|error| {
            MoaError::StorageError(format!("invalid contact scope `{value}`: {error}"))
        })
}

/// Column projection shared by every full artifact-revision load.
///
/// The order here must stay in lockstep with [`revision_from_row`], which reads
/// each column by name; keep both in sync when columns are added or removed.
pub(super) const REVISION_COLUMNS: &str = "a.artifact_uid, r.revision_uid, a.storage_partition_id, a.user_id, a.scope, \
     a.kind, a.name, a.description, a.tags, r.definition, \
     r.canonical_hash, r.source_format, r.source_text, r.status, \
     r.validation_report, r.version, r.published_at, r.valid_to, \
     r.created_at, r.updated_at";

pub(super) fn revision_from_row(row: &sqlx::postgres::PgRow) -> Result<StoredArtifactRevision> {
    let kind_text: String = row.try_get("kind").map_err(map_sqlx_error)?;
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    let definition: Value = row.try_get("definition").map_err(map_sqlx_error)?;
    let (storage_partition_id, user_id, scope) = scope_from_columns(
        row.try_get("scope").map_err(map_sqlx_error)?,
        row.try_get("storage_partition_id")
            .map_err(map_sqlx_error)?,
        row.try_get("user_id").map_err(map_sqlx_error)?,
    )?;
    Ok(StoredArtifactRevision {
        artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
        revision_uid: row.try_get("revision_uid").map_err(map_sqlx_error)?,
        storage_partition_id,
        user_id,
        scope,
        kind: kind_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        name: row.try_get("name").map_err(map_sqlx_error)?,
        description: row.try_get("description").map_err(map_sqlx_error)?,
        tags: row
            .try_get::<Option<Vec<String>>, _>("tags")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        document: serde_json::from_value(definition)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
        canonical_hash: row.try_get("canonical_hash").map_err(map_sqlx_error)?,
        source_format: row.try_get("source_format").map_err(map_sqlx_error)?,
        source_text: row.try_get("source_text").map_err(map_sqlx_error)?,
        status: status_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        validation_report: row.try_get("validation_report").map_err(map_sqlx_error)?,
        version: row.try_get("version").map_err(map_sqlx_error)?,
        published_at: row.try_get("published_at").map_err(map_sqlx_error)?,
        valid_to: row.try_get("valid_to").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

pub(super) fn summary_from_row(row: &sqlx::postgres::PgRow) -> Result<ArtifactSummary> {
    let kind_text: String = row.try_get("kind").map_err(map_sqlx_error)?;
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    Ok(ArtifactSummary {
        artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
        revision_uid: row.try_get("revision_uid").map_err(map_sqlx_error)?,
        scope: row.try_get("scope").map_err(map_sqlx_error)?,
        kind: kind_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        name: row.try_get("name").map_err(map_sqlx_error)?,
        description: row.try_get("description").map_err(map_sqlx_error)?,
        tags: row
            .try_get::<Option<Vec<String>>, _>("tags")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        status: status_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        version: row.try_get("version").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

fn file_from_row(row: &sqlx::postgres::PgRow) -> Result<ArtifactFile> {
    Ok(ArtifactFile {
        file_uid: row.try_get("file_uid").map_err(map_sqlx_error)?,
        path: row.try_get("path").map_err(map_sqlx_error)?,
        content: row.try_get("content").map_err(map_sqlx_error)?,
        content_sha256: row.try_get("content_sha256").map_err(map_sqlx_error)?,
        content_type: row.try_get("content_type").map_err(map_sqlx_error)?,
        executable: row.try_get("executable").map_err(map_sqlx_error)?,
        file_size_bytes: row.try_get("file_size_bytes").map_err(map_sqlx_error)?,
    })
}
fn validate_source_format(source_format: &str) -> Result<()> {
    if matches!(source_format, "json" | "yaml") {
        return Ok(());
    }
    Err(MoaError::ValidationError(format!(
        "unsupported artifact source format: {source_format}"
    )))
}
