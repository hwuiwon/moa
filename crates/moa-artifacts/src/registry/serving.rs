//! Type-owned serving pointer reads.
//!
//! A normal runtime resolution asks "what does this tenant serve for this
//! artifact", which is a pointer lookup, not a status search. The queries here
//! read `moa.artifact_serving_pointer`, and nothing but the activation transaction
//! writes it.

use super::revisions::{REVISION_COLUMNS, revision_from_row, summary_from_row};
use super::*;
use crate::release::{ActivationTargetClass, Digest32, EvalOverlayBinding, TenantScope};

/// One tenant's serving pointer for one release-gated artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServingPointer {
    /// Artifact whose pointer this is.
    pub artifact_uid: Uuid,
    /// Artifact kind, which fixes the activation target class.
    pub kind: ArtifactKind,
    /// Revision currently served.
    pub revision_uid: Uuid,
    /// Version of the served revision.
    pub revision_version: i32,
    /// Canonical hash of the served revision, as recorded at activation.
    pub revision_hash: Digest32,
    /// Compare-and-set token every activation must match and increment.
    pub pointer_version: i64,
    /// Activation audit that installed this pointer.
    pub activation_audit_uid: Uuid,
    /// When this pointer was last moved.
    pub activated_at: DateTime<Utc>,
}

/// Immutable activation generation used to authorize a later rollback proposal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationProvenance {
    /// Artifact whose serving pointer moved.
    pub artifact_uid: Uuid,
    /// Revision installed by the activation.
    pub activated_revision_uid: Uuid,
    /// Revision that served immediately before this activation, when any.
    pub previous_revision_uid: Option<Uuid>,
    /// Monotonic serving-pointer version installed by the activation.
    pub activated_pointer_version: i64,
    /// Type-owned target whose serving state moved.
    pub activation_target: ActivationTargetClass,
}

impl ArtifactRegistry {
    /// Loads one exact tenant-scoped activation generation for provenance checks.
    pub async fn load_activation_provenance(
        &self,
        scope: &TenantScope,
        audit_uid: Uuid,
    ) -> Result<Option<ActivationProvenance>> {
        let mut conn = ScopedConn::begin(
            &self.pool,
            &artifact_scope_context(&scope.action_rule_scope()),
        )
        .await?;
        let row = sqlx::query(
            r#"
            SELECT artifact_uid, activated_revision_uid, previous_revision_uid,
                   activated_pointer_version, activation_target
            FROM moa.artifact_activation_audit
            WHERE audit_uid = $1
              AND storage_partition_id = $2
              AND decision_kind = 'activation'
            "#,
        )
        .bind(audit_uid)
        .bind(scope.storage_partition_id().to_string())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.map(|row| {
            Ok(ActivationProvenance {
                artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
                activated_revision_uid: row
                    .try_get("activated_revision_uid")
                    .map_err(map_sqlx_error)?,
                previous_revision_uid: row
                    .try_get("previous_revision_uid")
                    .map_err(map_sqlx_error)?,
                activated_pointer_version: row
                    .try_get("activated_pointer_version")
                    .map_err(map_sqlx_error)?,
                activation_target: row
                    .try_get::<String, _>("activation_target")
                    .map_err(map_sqlx_error)?
                    .parse()
                    .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
            })
        })
        .transpose()
    }

    /// Loads the exact revision a tenant serves for one named artifact.
    ///
    /// This is the normal runtime resolver for release-gated kinds. It answers
    /// only from the serving pointer, so a draft, evaluating, ready, rejected,
    /// inconclusive, superseded, or archived revision is never resolvable.
    pub async fn load_serving(
        &self,
        scope: &ActionRuleScope,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<Option<StoredArtifactRevision>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(&format!(
            r#"
            SELECT {REVISION_COLUMNS}
            FROM moa.artifact_serving_pointer p
            JOIN moa.artifact a ON a.artifact_uid = p.artifact_uid
            JOIN moa.artifact_revision r ON r.revision_uid = p.revision_uid
            WHERE a.valid_to IS NULL
              AND r.valid_to IS NULL
              AND a.kind = $2
              AND a.name = $3
              AND p.storage_partition_id = $1
            LIMIT 1
            "#
        ))
        .bind(parts.storage_partition_id.as_deref())
        .bind(kind.to_string())
        .bind(name)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(revision_from_row).transpose()
    }

    /// Resolves an artifact under an evaluation overlay, falling back to the pointer.
    ///
    /// This is the seam that makes a release evaluation actually evaluate the
    /// candidate. Without it the dispatch runs against whatever the tenant already
    /// serves, so the gate would decide on the wrong artifact while every predicate
    /// around it looked correct.
    ///
    /// The overlay is consulted first and only ever *substitutes*: an artifact the
    /// submitter did not pin still resolves through the serving pointer, so an
    /// evaluation sees its candidate plus the tenant's real dependencies rather than
    /// a wholly synthetic world. Authority stays in
    /// `moa.resolve_release_overlay_revision`, which requires the overlay's secret
    /// and the eval-owned session bound to it and stops answering once the overlay
    /// closes or expires — this function holds no bypass of its own.
    pub async fn load_serving_with_overlay(
        &self,
        scope: &ActionRuleScope,
        kind: ArtifactKind,
        name: &str,
        overlay: Option<&EvalOverlayBinding>,
    ) -> Result<Option<StoredArtifactRevision>> {
        let Some(overlay) = overlay else {
            return self.load_serving(scope, kind, name).await;
        };
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(&format!(
            r#"
            SELECT {REVISION_COLUMNS}
            FROM moa.artifact a
            JOIN moa.artifact_revision r
              ON r.artifact_uid = a.artifact_uid
             AND r.revision_uid = COALESCE(
                    moa.resolve_release_overlay_revision($4, $5, $6, a.artifact_uid, $7),
                    (
                        SELECT p.revision_uid
                        FROM moa.artifact_serving_pointer p
                        WHERE p.artifact_uid = a.artifact_uid
                          AND p.storage_partition_id = $1
                    )
                )
            WHERE a.valid_to IS NULL
              AND r.valid_to IS NULL
              AND a.storage_partition_id = $1
              AND a.kind = $2
              AND a.name = $3
            LIMIT 1
            "#
        ))
        .bind(parts.storage_partition_id.as_deref())
        .bind(kind.to_string())
        .bind(name)
        .bind(overlay.overlay_uid)
        .bind(overlay.token_hash().to_vec())
        .bind(overlay.eval_session_id)
        .bind(Utc::now())
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(revision_from_row).transpose()
    }

    /// Loads the exact primary artifact bound to an evaluation overlay.
    ///
    /// This is used only to bind a release candidate into the platform-owned
    /// evaluation host. The same secret, eval-session, expiry, and closed-state
    /// checks as symbolic overlay substitution apply; the overlay row alone is
    /// not sufficient authority.
    pub async fn load_release_overlay_target(
        &self,
        scope: &ActionRuleScope,
        overlay: &EvalOverlayBinding,
    ) -> Result<Option<StoredArtifactRevision>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(&format!(
            r#"
            SELECT {REVISION_COLUMNS}
            FROM moa.artifact_release_eval_overlay overlay
            JOIN moa.artifact a ON a.artifact_uid = overlay.artifact_uid
            JOIN moa.artifact_revision r
              ON r.artifact_uid = a.artifact_uid
             AND r.revision_uid = moa.resolve_release_overlay_revision(
                    overlay.overlay_uid, $2, $3, overlay.artifact_uid, $4
                )
            WHERE overlay.overlay_uid = $1
              AND overlay.storage_partition_id = $5
              AND a.valid_to IS NULL
              AND r.valid_to IS NULL
            "#
        ))
        .bind(overlay.overlay_uid)
        .bind(overlay.token_hash().to_vec())
        .bind(overlay.eval_session_id)
        .bind(Utc::now())
        .bind(parts.storage_partition_id.as_deref())
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(revision_from_row).transpose()
    }

    /// Lists every artifact a tenant currently serves for one kind.
    pub async fn list_serving(
        &self,
        scope: &ActionRuleScope,
        kind: ArtifactKind,
    ) -> Result<Vec<ArtifactSummary>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let rows = sqlx::query(
            r#"
            SELECT a.artifact_uid, r.revision_uid, a.scope, a.kind, a.name,
                   a.description, a.tags, r.status, r.version, r.updated_at
            FROM moa.artifact_serving_pointer p
            JOIN moa.artifact a ON a.artifact_uid = p.artifact_uid
            JOIN moa.artifact_revision r ON r.revision_uid = p.revision_uid
            WHERE a.valid_to IS NULL
              AND r.valid_to IS NULL
              AND a.kind = $2
              AND p.storage_partition_id = $1
            ORDER BY a.name ASC
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(kind.to_string())
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(summary_from_row).collect()
    }

    /// Loads the serving pointer for one artifact.
    pub async fn load_serving_pointer(
        &self,
        scope: &TenantScope,
        artifact_uid: Uuid,
    ) -> Result<Option<ServingPointer>> {
        let mut conn = ScopedConn::begin(
            &self.pool,
            &artifact_scope_context(&scope.action_rule_scope()),
        )
        .await?;
        let pointer = load_serving_pointer_in_tx(conn.as_mut(), scope, artifact_uid, false).await?;
        conn.commit().await?;
        Ok(pointer)
    }

    /// Returns whether a revision was ever activated by the release path.
    ///
    /// A session pinned to an exact revision keeps working after a newer
    /// revision activates, because the pinned one demonstrably passed the gate and
    /// served. A candidate that was superseded in the coalescing slot without ever
    /// serving has no audit row, so it stays unloadable.
    pub async fn was_ever_activated(
        &self,
        scope: &TenantScope,
        revision_uid: Uuid,
    ) -> Result<bool> {
        let mut conn = ScopedConn::begin(
            &self.pool,
            &artifact_scope_context(&scope.action_rule_scope()),
        )
        .await?;
        let activated = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM moa.artifact_activation_audit
                WHERE activated_revision_uid = $1
                  AND storage_partition_id = $2
            )
            "#,
        )
        .bind(revision_uid)
        .bind(scope.storage_partition_id().to_string())
        .fetch_one(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok(activated)
    }
}

/// Loads a serving pointer inside the caller's transaction.
///
/// `for_update` takes the row lock the activation transaction needs so two
/// concurrent activations of the same artifact serialize on the pointer.
pub(crate) async fn load_serving_pointer_in_tx(
    conn: &mut PgConnection,
    scope: &TenantScope,
    artifact_uid: Uuid,
    for_update: bool,
) -> Result<Option<ServingPointer>> {
    let statement = format!(
        r#"
        SELECT p.artifact_uid, p.kind, p.revision_uid, p.revision_version, p.revision_hash,
               p.pointer_version, p.activated_at,
               (
                   SELECT audit.audit_uid
                   FROM moa.artifact_activation_audit audit
                   WHERE audit.artifact_uid = p.artifact_uid
                     AND audit.decision_kind = 'activation'
                     AND audit.activated_revision_uid = p.revision_uid
                     AND audit.activated_pointer_version = p.pointer_version
                   LIMIT 1
               ) AS activation_audit_uid
        FROM moa.artifact_serving_pointer p
        WHERE p.artifact_uid = $1
          AND p.storage_partition_id = $2
        {}
        "#,
        if for_update { "FOR UPDATE" } else { "" }
    );
    let row = sqlx::query(&statement)
        .bind(artifact_uid)
        .bind(scope.storage_partition_id().to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let kind_text: String = row.try_get("kind").map_err(map_sqlx_error)?;
    let hash: Vec<u8> = row.try_get("revision_hash").map_err(map_sqlx_error)?;
    Ok(Some(ServingPointer {
        artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
        kind: kind_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        revision_uid: row.try_get("revision_uid").map_err(map_sqlx_error)?,
        revision_version: row.try_get("revision_version").map_err(map_sqlx_error)?,
        revision_hash: Digest32::from_slice(&hash)
            .map_err(|error| MoaError::StorageError(error.to_string()))?,
        pointer_version: row.try_get("pointer_version").map_err(map_sqlx_error)?,
        activation_audit_uid: row
            .try_get("activation_audit_uid")
            .map_err(map_sqlx_error)?,
        activated_at: row.try_get("activated_at").map_err(map_sqlx_error)?,
    }))
}
