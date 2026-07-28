//! The single SQL enforcement point for provider-native source ACL admission.
//!
//! Retrieval reaches tenant knowledge through five independent paths — pgvector
//! KNN, Postgres lexical search, recursive graph expansion, chunk hydration, and
//! context-window neighbour expansion — plus an external vector backend that
//! answers outside Postgres entirely. Each one could leak a document that the
//! source system does not share with the caller, so each one carries the same
//! predicate, built here rather than transcribed six times.
//!
//! The predicate answers one question about a candidate graph node uid: *is this
//! node governed by a source ACL that refuses this caller?* It is deliberately
//! shaped as a `NOT EXISTS` over the governing object so that a node which is
//! not source-governed at all (ordinary contact memory, an entity, a fact) costs
//! one index probe and is admitted, while a governed node must satisfy the full
//! mode/state/revision/allow/deny rule.
//!
//! Principals arrive as bind parameters — opaque keyed fingerprints resolved
//! once per request by [`resolve_source_acl_context`]. They are never read from
//! request JSON, never re-fetched inside a leg, and never interpolated into SQL.

use moa_core::{
    error::Result,
    types::contact::ContactId,
    types::identifiers::TenantId,
    types::memory::{RlsContext, SourceAclContext, SourcePrincipalFingerprint},
};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::{ScopedConn, map_sqlx_error};

/// Maximum principals carried in one caller admission context.
///
/// A caller with more bound principals than this is truncated in canonical
/// fingerprint order, which can only narrow what they see. The cap keeps the
/// `= ANY($1)` bind parameter bounded so one pathological directory expansion
/// cannot turn every retrieval leg into a large-array scan.
pub const MAX_SOURCE_ACL_PRINCIPALS: usize = 512;

/// Sentinel contact id for principals every member of a tenant holds.
///
/// Provider "anyone with access" grants are bound once per connection under this
/// id instead of being fanned out to every contact row, so a new member inherits
/// them without a backfill.
pub const TENANT_WIDE_PRINCIPAL_HOLDER: uuid::Uuid = uuid::Uuid::nil();

/// Appends the source-ACL admission predicate for one candidate uid expression.
///
/// `uid_expr` must be a literal column reference the caller controls (for
/// example `"node.uid"` or `"embedding.uid"`); it is spliced into SQL verbatim
/// and must never carry user input.
///
/// The emitted predicate is a complete boolean expression, so callers append it
/// with an explicit `AND`.
pub fn push_source_acl_predicate(
    builder: &mut QueryBuilder<'_, Postgres>,
    uid_expr: &str,
    acl: &SourceAclContext,
) {
    let principals = acl.bind_values();
    builder.push(
        r#"NOT EXISTS (
            SELECT 1
            FROM (
                SELECT acl_version.object_id AS object_id
                FROM moa.knowledge_chunks AS acl_chunk
                JOIN moa.knowledge_document_versions AS acl_version
                  ON acl_version.document_version_uid = acl_chunk.document_version_id
                WHERE acl_chunk.chunk_uid = "#,
    );
    builder.push(uid_expr);
    builder.push(
        r#"
                UNION ALL
                SELECT acl_doc_version.object_id AS object_id
                FROM moa.knowledge_document_versions AS acl_doc_version
                WHERE acl_doc_version.graph_node_uid = "#,
    );
    builder.push(uid_expr);
    builder.push(
        r#"
            ) AS acl_governed
            JOIN moa.knowledge_objects AS acl_object
              ON acl_object.object_uid = acl_governed.object_id
            JOIN moa.knowledge_connections AS acl_connection
              ON acl_connection.connection_uid = acl_object.connection_id
            WHERE NOT (
                acl_connection.acl_mode = 'tenant_public'
                OR (
                    acl_connection.acl_mode = 'provider_managed'
                    AND acl_object.acl_state = 'current'
                    AND EXISTS (
                        SELECT 1
                        FROM moa.knowledge_source_acl_snapshots AS acl_snapshot
                        WHERE acl_snapshot.snapshot_uid = acl_object.current_acl_snapshot_id
                          AND acl_snapshot.object_id = acl_object.object_uid
                          AND acl_snapshot.complete
                          AND acl_snapshot.provider_revision = acl_object.acl_revision
                    )
                    AND EXISTS (
                        SELECT 1
                        FROM moa.knowledge_source_acl_entries AS acl_allow
                        WHERE acl_allow.snapshot_id = acl_object.current_acl_snapshot_id
                          AND acl_allow.entry_kind = 'allow'
                          AND acl_allow.principal_fingerprint = ANY("#,
    );
    builder.push_bind(principals.clone());
    builder.push(
        r#")
                    )
                    AND NOT EXISTS (
                        SELECT 1
                        FROM moa.knowledge_source_acl_entries AS acl_deny
                        WHERE acl_deny.snapshot_id = acl_object.current_acl_snapshot_id
                          AND acl_deny.entry_kind = 'deny'
                          AND acl_deny.principal_fingerprint = ANY("#,
    );
    builder.push_bind(principals);
    builder.push(
        r#")
                    )
                )
            )
        )"#,
    );
}

/// Resolves the caller's bounded, canonical source-ACL admission context.
///
/// Reads only durable state: principals verified for this contact, the
/// tenant-wide principals every member holds, and one level of group/domain
/// expansion over those. Nested groups must be flattened by the adapter when the
/// binding is written, so this stays a single bounded query on the request path.
///
/// The tenant's current ACL epoch is read in the same transaction as the
/// bindings, so the returned context describes one consistent moment; a
/// concurrent permission change produces a higher epoch and misses the cache
/// rather than reusing this one.
pub async fn resolve_source_acl_context(
    pool: &PgPool,
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    assume_app_role: bool,
) -> Result<SourceAclContext> {
    let mut conn =
        ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), assume_app_role).await?;

    let epoch = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(
            (SELECT epoch FROM moa.knowledge_source_acl_epochs WHERE tenant_id = $1),
            0
        )
        "#,
    )
    .bind(tenant_id.0)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    let rows = sqlx::query_scalar::<_, Vec<u8>>(
        r#"
        WITH direct AS (
            SELECT principal_fingerprint
            FROM moa.knowledge_source_principal_bindings
            WHERE tenant_id = $1
              AND (contact_id = $2 OR contact_id = $3)
        ),
        expanded AS (
            SELECT groups.group_fingerprint AS principal_fingerprint
            FROM moa.knowledge_source_principal_group_bindings AS groups
            JOIN direct ON direct.principal_fingerprint = groups.member_fingerprint
            WHERE groups.tenant_id = $1
        )
        SELECT principal_fingerprint FROM direct
        UNION
        SELECT principal_fingerprint FROM expanded
        ORDER BY 1
        LIMIT $4
        "#,
    )
    .bind(tenant_id.0)
    .bind(contact_id.map_or(TENANT_WIDE_PRINCIPAL_HOLDER, |contact| contact.0))
    .bind(TENANT_WIDE_PRINCIPAL_HOLDER)
    .bind(MAX_SOURCE_ACL_PRINCIPALS as i64)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await?;

    if rows.len() == MAX_SOURCE_ACL_PRINCIPALS {
        tracing::warn!(
            tenant_id = %tenant_id,
            limit = MAX_SOURCE_ACL_PRINCIPALS,
            "caller source-ACL principal set hit the bound; extra principals are not admitted"
        );
    }

    let principals = rows
        .iter()
        .map(|bytes| SourcePrincipalFingerprint::from_bytes(bytes))
        .collect::<Result<Vec<_>>>()?;
    Ok(SourceAclContext::new(principals, epoch))
}

/// Reads the tenant's current source-ACL epoch.
///
/// Used by cache freshness checks that already hold a scoped connection budget
/// and only need the epoch, not the caller's principals.
pub async fn current_source_acl_epoch(
    pool: &PgPool,
    tenant_id: TenantId,
    assume_app_role: bool,
) -> Result<i64> {
    let mut conn =
        ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), assume_app_role).await?;
    let epoch = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(
            (SELECT epoch FROM moa.knowledge_source_acl_epochs WHERE tenant_id = $1),
            0
        )
        "#,
    )
    .bind(tenant_id.0)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await?;
    Ok(epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::memory::SourcePrincipalFingerprint;

    #[test]
    fn predicate_binds_principals_and_never_interpolates_them() {
        // Pins: the caller's fingerprints reach SQL as bind parameters only, the
        // deny anti-join is present, and the governed-node resolution covers both
        // chunk occurrences and document nodes.
        let acl = SourceAclContext::new(
            [
                SourcePrincipalFingerprint::from_digest(1, [7; 32]),
                SourcePrincipalFingerprint::from_digest(1, [9; 32]),
            ],
            42,
        );
        let mut builder = QueryBuilder::<Postgres>::new("SELECT 1 WHERE ");
        push_source_acl_predicate(&mut builder, "node.uid", &acl);
        let sql = builder.into_sql();

        assert!(sql.contains("moa.knowledge_chunks"));
        assert!(sql.contains("acl_doc_version.graph_node_uid = node.uid"));
        assert!(sql.contains("acl_connection.acl_mode = 'tenant_public'"));
        assert!(sql.contains("acl_object.acl_state = 'current'"));
        assert!(sql.contains("acl_snapshot.provider_revision = acl_object.acl_revision"));
        assert!(sql.contains("acl_snapshot.complete"));
        assert!(sql.contains("acl_allow.entry_kind = 'allow'"));
        assert!(sql.contains("AND NOT EXISTS"), "deny anti-join is required");
        assert!(sql.contains("acl_deny.entry_kind = 'deny'"));
        assert_eq!(
            sql.matches("= ANY($").count(),
            2,
            "allow and deny each bind the principal array"
        );
        assert!(
            !sql.contains("\\x") && !sql.contains("07070707"),
            "fingerprints must never be interpolated: {sql}"
        );
    }

    #[test]
    fn empty_principal_set_still_emits_the_bind_and_denies() {
        // Pins: a caller with no resolved principals produces an empty array
        // bind, so `= ANY` is false and provider-managed content is denied — it
        // must not degrade into an omitted predicate.
        let acl = SourceAclContext::empty(0);
        assert!(acl.bind_values().is_empty());
        let mut builder = QueryBuilder::<Postgres>::new("SELECT 1 WHERE ");
        push_source_acl_predicate(&mut builder, "embedding.uid", &acl);
        let sql = builder.into_sql();
        assert!(sql.contains("acl_allow.entry_kind = 'allow'"));
        assert_eq!(sql.matches("= ANY($").count(), 2);
    }
}
