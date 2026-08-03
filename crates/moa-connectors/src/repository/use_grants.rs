//! Direct connector use-relationship persistence.

use super::model::{assume_owner_role, lock_connection};
use super::*;

#[derive(FromRow)]
struct ConnectionUseGrantRow {
    subject_kind: String,
    subject_id: Uuid,
}

#[async_trait]
impl ConnectionUseGrantRepository for PostgresConnectionRepository {
    async fn grant_use(&self, request: ConnectionUseRequest) -> Result<()> {
        let mut conn = self.begin(request.tenant_id).await?;
        let connection =
            lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        if matches!(
            connection.status,
            ConnectionStatus::Disconnecting | ConnectionStatus::Deleted
        ) {
            return Err(Error::UseGrantConnectionUnavailable {
                connection_id: request.connection_id,
                status: connection.status,
            });
        }
        validate_use_subject(&mut conn, request.tenant_id, request.subject, true).await?;
        sqlx::query(
            "INSERT INTO moa.connector_connection_use_grants \
             (tenant_id, connection_uid, subject_kind, subject_id) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (tenant_id, connection_uid, subject_kind, subject_id) DO NOTHING",
        )
        .bind(request.tenant_id.0)
        .bind(request.connection_id.0)
        .bind(request.subject.kind())
        .bind(request.subject.id())
        .execute(conn.as_mut())
        .await?;

        assume_owner_role(&mut conn).await?;
        enqueue(
            conn.as_mut(),
            TupleOp::Write,
            &request.subject.tuple(request.connection_id),
            Some(request.tenant_id.0),
        )
        .await?;
        conn.commit().await?;
        Ok(())
    }

    async fn revoke_use(&self, request: ConnectionUseRequest) -> Result<()> {
        let mut conn = self.begin(request.tenant_id).await?;
        lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        validate_use_subject(&mut conn, request.tenant_id, request.subject, false).await?;
        sqlx::query(
            "DELETE FROM moa.connector_connection_use_grants \
             WHERE tenant_id = $1 AND connection_uid = $2 \
               AND subject_kind = $3 AND subject_id = $4",
        )
        .bind(request.tenant_id.0)
        .bind(request.connection_id.0)
        .bind(request.subject.kind())
        .bind(request.subject.id())
        .execute(conn.as_mut())
        .await?;

        assume_owner_role(&mut conn).await?;
        enqueue(
            conn.as_mut(),
            TupleOp::Delete,
            &request.subject.tuple(request.connection_id),
            Some(request.tenant_id.0),
        )
        .await?;
        conn.commit().await?;
        Ok(())
    }
}

async fn validate_use_subject(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    subject: ConnectorUseSubject,
    require_active: bool,
) -> Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT moa.connector_use_subject_exists($1, $2, $3)")
        .bind(tenant_id.0)
        .bind(subject.kind())
        .bind(subject.id())
        .fetch_one(conn.as_mut())
        .await?;
    if !exists {
        return Err(Error::UseGrantSubjectNotFound {
            subject_kind: subject.kind(),
            subject_id: subject.id(),
        });
    }
    let eligible = if require_active {
        sqlx::query_scalar("SELECT moa.connector_use_subject_is_eligible($1, $2, $3)")
            .bind(tenant_id.0)
            .bind(subject.kind())
            .bind(subject.id())
            .fetch_one(conn.as_mut())
            .await?
    } else {
        true
    };
    if !eligible {
        return Err(Error::UseGrantSubjectInactive {
            subject_kind: subject.kind(),
            subject_id: subject.id(),
        });
    }
    Ok(())
}

pub(super) async fn take_connection_use_grants(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Result<Vec<ConnectorUseSubject>> {
    let rows = sqlx::query_as::<_, ConnectionUseGrantRow>(
        "SELECT subject_kind, subject_id FROM moa.connector_connection_use_grants \
         WHERE tenant_id = $1 AND connection_uid = $2 \
         ORDER BY subject_kind, subject_id",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .fetch_all(conn.as_mut())
    .await?;
    let subjects = rows
        .into_iter()
        .map(|row| match row.subject_kind.as_str() {
            "operator" => Ok(ConnectorUseSubject::Operator { id: row.subject_id }),
            "agent" => Ok(ConnectorUseSubject::Agent { id: row.subject_id }),
            "contact" => Ok(ConnectorUseSubject::Contact { id: row.subject_id }),
            _ => Err(Error::CatalogInvariant {
                message: "persisted connector Use subject kind is unknown".to_string(),
            }),
        })
        .collect::<Result<Vec<_>>>()?;
    sqlx::query(
        "DELETE FROM moa.connector_connection_use_grants \
         WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .execute(conn.as_mut())
    .await?;
    Ok(subjects)
}
