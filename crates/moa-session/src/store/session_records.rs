//! Session metadata and contact record persistence.

use super::*;

fn validate_session_create_meta(meta: &SessionMeta) -> Result<()> {
    if meta.contact.is_none() && meta.created_by.is_none() {
        return Err(MoaError::ValidationError(
            "session creation requires contact or creator attribution".to_string(),
        ));
    }
    if let Some(contact) = &meta.contact
        && contact.tenant_id != meta.tenant_id
    {
        return Err(MoaError::ValidationError(
            "session contact tenant_id must match session tenant_id".to_string(),
        ));
    }
    if meta.agent_context.is_none() {
        return Err(MoaError::ValidationError(
            "session creation requires a pinned agent_context".to_string(),
        ));
    }
    Ok(())
}

/// Outcome of an idempotent session insert in a caller-owned transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCreateOutcome {
    /// The session identity that now exists (whether inserted here or already present).
    pub session_id: moa_core::types::identifiers::SessionId,
    /// `true` when this call inserted the row; `false` when a row with the same
    /// id already existed (a replay of a committed creation).
    pub inserted: bool,
}

impl PostgresSessionStore {
    /// Insert a session metadata row using a caller-owned transaction.
    ///
    /// This lets higher-level handlers atomically persist the session and its
    /// authorization outbox tuples. The insert is idempotent on the session id
    /// (`ON CONFLICT (id) DO NOTHING`): a replay that reuses a replay-stable id
    /// finds the row already present and reports `inserted = false` without
    /// duplicating the row or its agent sidecar. The caller owns commit/rollback
    /// and should gate any dependent writes on
    /// [`SessionCreateOutcome::inserted`], then call
    /// [`PostgresSessionStore::refresh_active_session_metric`] after a successful
    /// commit.
    pub async fn create_session_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        meta: SessionMeta,
    ) -> Result<SessionCreateOutcome> {
        validate_session_create_meta(&meta)?;
        let session_id = meta.id;
        let tenant_id = meta.tenant_id;
        let tenant_storage_key = StoragePartitionId::for_tenant(tenant_id);
        let actor_storage_key = session_actor_storage_key(meta.created_by.as_ref());
        let status = meta.status.clone();
        let agent_context = meta.agent_context.clone();
        let sessions = self.table_name("sessions");
        let insert_result = sqlx::query(&format!(
            "INSERT INTO {sessions} ({SESSION_INSERT_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30) \
             ON CONFLICT (id) DO NOTHING"
        ))
        .bind(session_id.0)
        .bind(tenant_id.0)
        .bind(tenant_storage_key.as_str())
        .bind(actor_storage_key.as_str())
        .bind(meta.title)
        .bind(meta.status.as_str())
        .bind(meta.channel.as_str())
        .bind(meta.active_channel_binding_id.map(|id| id.0))
        .bind(meta.model.to_string())
        .bind(meta.created_at)
        .bind(meta.updated_at)
        .bind(meta.completed_at)
        .bind(meta.parent_session_id.map(|value| value.0))
        .bind(meta.contact.as_ref().map(|contact| contact.contact_id.0))
        .bind(meta.contact.as_ref().map(|contact| contact.tenant_id.0))
        .bind(meta.contact.as_ref().map(|contact| contact.state.as_str()))
        .bind(
            meta.contact
                .as_ref()
                .and_then(|contact| contact.canonical_contact_id.map(|id| id.0)),
        )
        .bind(
            meta.contact
                .as_ref()
                .map(|contact| {
                    contact
                        .linked_contact_ids
                        .iter()
                        .map(|id| id.0)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
        .bind(
            meta.contact
                .as_ref()
                .map(|contact| contact.scopes.clone())
                .unwrap_or_default(),
        )
        .bind(meta.created_by.as_ref().map(session_actor_type))
        .bind(meta.created_by.as_ref().and_then(session_actor_id))
        .bind(meta.contact_promoted_from_id.map(|id| id.0))
        .bind(meta.total_input_tokens_uncached as i64)
        .bind(meta.total_input_tokens_cache_write as i64)
        .bind(meta.total_input_tokens_cache_read as i64)
        .bind(meta.total_output_tokens as i64)
        .bind(meta.total_cost_cents as i64)
        .bind(meta.event_count as i64)
        .bind(0_i64)
        .bind(meta.last_checkpoint_seq.map(|value| value as i64))
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?;
        let inserted = insert_result.rows_affected() == 1;
        // A conflict means a committed creation is being replayed with the same
        // replay-stable id; leave the existing row and its sidecar untouched so
        // the caller can short-circuit dependent writes.
        if inserted {
            if let Some(agent_context) = agent_context.as_ref() {
                self.insert_session_agent_context_in_tx(
                    tx,
                    session_id,
                    tenant_id,
                    actor_storage_key.as_str(),
                    agent_context,
                )
                .await?;
            }
            record_session_created(&tenant_id, &status);
        }

        Ok(SessionCreateOutcome {
            session_id,
            inserted,
        })
    }

    async fn insert_session_agent_context_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        session_id: moa_core::types::identifiers::SessionId,
        tenant_id: moa_core::types::identifiers::TenantId,
        actor_storage_key: &str,
        context: &moa_core::types::agent::AgentContext,
    ) -> Result<()> {
        let table = self.table_name("session_agent_context");
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
        sqlx::query(&format!(
            r#"
            INSERT INTO {table} (
                session_id, storage_partition_id, user_id, agent_id, installation_uid,
                deployment_uid, agent_definition_ref, agent_revision_uid,
                policy_hash, display_name, policy_snapshot, artifact_dependencies,
                tool_dependencies
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#
        ))
        .bind(session_id.0)
        .bind(storage_partition_id.as_str())
        .bind(actor_storage_key)
        .bind(context.agent_id)
        .bind(context.installation_uid)
        .bind(context.deployment_uid)
        .bind(&context.definition_ref)
        .bind(context.revision_uid)
        .bind(&context.policy_hash)
        .bind(&context.display_name)
        .bind(&context.policy_snapshot)
        .bind(Json(&context.artifact_dependencies))
        .bind(Json(&context.tool_dependencies))
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }
    /// Updates contact metadata attached to an existing session.
    pub async fn update_session_contact(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        contact: moa_core::types::contact::ContactRef,
        promoted_from: Option<moa_core::types::contact::ContactId>,
    ) -> Result<()> {
        let sessions = self.table_name("sessions");
        let affected = sqlx::query(&format!(
            "UPDATE {sessions} SET \
                 contact_id = $1, \
                 contact_tenant_id = $2, \
                 contact_state = $3, \
                 contact_canonical_id = $4, \
                 contact_linked_ids = $5, \
                 contact_scopes = $6, \
                 contact_promoted_from_id = $7, \
                 updated_at = $8 \
             WHERE id = $9"
        ))
        .bind(contact.contact_id.0)
        .bind(contact.tenant_id.0)
        .bind(contact.state.as_str())
        .bind(contact.canonical_contact_id.map(|id| id.0))
        .bind(
            contact
                .linked_contact_ids
                .iter()
                .map(|id| id.0)
                .collect::<Vec<_>>(),
        )
        .bind(contact.scopes)
        .bind(promoted_from.map(|id| id.0))
        .bind(Utc::now())
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::SessionNotFound(session_id));
        }

        Ok(())
    }
}

pub(super) fn agent_context_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<Option<moa_core::types::agent::AgentContext>> {
    let Some(revision_uid) = row.col::<Option<Uuid>>("agent_revision_uid")? else {
        return Ok(None);
    };

    let artifact_dependencies = row
        .col::<Json<Vec<moa_core::types::agent::ResolvedArtifactRevisionRef>>>(
            "artifact_dependencies",
        )?
        .0;
    let tool_dependencies = row
        .col::<Json<Vec<moa_core::types::agent::LockedToolRef>>>("tool_dependencies")?
        .0;

    Ok(Some(moa_core::types::agent::AgentContext {
        agent_id: row.col::<Option<Uuid>>("agent_id")?,
        installation_uid: row.col::<Option<Uuid>>("installation_uid")?,
        deployment_uid: row.col::<Option<Uuid>>("deployment_uid")?,
        definition_ref: row.col::<String>("agent_definition_ref")?,
        revision_uid,
        policy_hash: row.col::<String>("policy_hash")?,
        display_name: row.col::<String>("display_name")?,
        artifact_dependencies,
        tool_dependencies,
        policy_snapshot: row.col::<serde_json::Value>("policy_snapshot")?,
    }))
}

pub(super) fn session_actor_type(
    actor: &moa_core::types::contact::SessionActorRef,
) -> &'static str {
    match actor {
        moa_core::types::contact::SessionActorRef::Identity { .. } => "identity",
        moa_core::types::contact::SessionActorRef::Contact { .. } => "contact",
        moa_core::types::contact::SessionActorRef::Anonymous => "anonymous",
    }
}

pub(super) fn session_actor_id(actor: &moa_core::types::contact::SessionActorRef) -> Option<Uuid> {
    match actor {
        moa_core::types::contact::SessionActorRef::Identity { id } => Some(*id),
        moa_core::types::contact::SessionActorRef::Contact { id } => Some(id.0),
        moa_core::types::contact::SessionActorRef::Anonymous => None,
    }
}

fn session_actor_storage_key(actor: Option<&moa_core::types::contact::SessionActorRef>) -> String {
    match actor {
        Some(moa_core::types::contact::SessionActorRef::Identity { id }) => {
            format!("identity:{id}")
        }
        Some(moa_core::types::contact::SessionActorRef::Contact { id }) => format!("contact:{id}"),
        Some(moa_core::types::contact::SessionActorRef::Anonymous) => "anonymous".to_string(),
        None => "system".to_string(),
    }
}
