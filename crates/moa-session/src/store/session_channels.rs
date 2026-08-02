//! Session channel binding persistence.

use super::*;

impl PostgresSessionStore {
    /// Replaces a session's active channel binding and current channel metadata.
    ///
    /// Allocates a fresh binding id and applies the replacement in its own
    /// transaction. Handlers that must bind the channel change atomically with an
    /// event or session creation should instead pass a replay-stable id to
    /// [`PostgresSessionStore::replace_session_channel_binding_in_tx`].
    pub async fn replace_session_channel_binding(
        &self,
        replacement: SessionChannelBindingReplacement<'_>,
    ) -> Result<moa_core::types::channel::SessionChannelBindingId> {
        let binding_id = moa_core::types::channel::SessionChannelBindingId::new();
        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;
        self.replace_session_channel_binding_in_tx(&mut tx, binding_id, replacement)
            .await?;
        tx.commit().await.map_err(map_sqlx_error)?;
        Ok(binding_id)
    }

    /// Replaces a session's active channel binding within a caller-owned
    /// transaction, using a caller-supplied binding id.
    ///
    /// The new-binding insert is idempotent on the binding id
    /// (`ON CONFLICT (id) DO NOTHING`). A replay that reuses a replay-stable id
    /// finds the binding already present, makes no further changes, and returns
    /// `false`, so ending prior bindings and repointing the session run exactly
    /// once. A fresh insert ends any other still-open bindings, repoints the
    /// session to the new binding, and returns `true`. Returns
    /// [`MoaError::SessionNotFound`] when the session row is absent.
    pub async fn replace_session_channel_binding_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        binding_id: moa_core::types::channel::SessionChannelBindingId,
        replacement: SessionChannelBindingReplacement<'_>,
    ) -> Result<bool> {
        let route = serde_json::to_value(replacement.channel_ref)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        let route_keys = channel_route_keys(replacement.channel_ref);
        let sessions = self.table_name("sessions");
        let bindings = self.table_name("session_channel_bindings");

        // End any other still-open binding first so the partial unique
        // "one active binding per session" index is satisfied when the new
        // active binding is inserted. On a replay the new binding is already the
        // only active one (its id is excluded), so this affects no rows.
        sqlx::query(&format!(
            "UPDATE {bindings} \
             SET ended_at = NOW(), last_used_at = NOW() \
             WHERE session_id = $1 AND ended_at IS NULL AND id <> $2"
        ))
        .bind(replacement.session_id.0)
        .bind(binding_id.0)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?;

        let inserted = sqlx::query(&format!(
            "INSERT INTO {bindings} \
                 (id, tenant_id, storage_partition_id, session_id, contact_id, channel_account_id, \
                  contact_point_id, channel, external_tenant_key, external_conversation_key, \
                  external_thread_key, route, reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             ON CONFLICT (id) DO NOTHING"
        ))
        .bind(binding_id.0)
        .bind(replacement.tenant_id.0)
        .bind(replacement.storage_partition_id.as_str())
        .bind(replacement.session_id.0)
        .bind(replacement.contact_id.0)
        .bind(replacement.channel_account_id.map(|id| id.0))
        .bind(replacement.contact_point_id.map(|id| id.0))
        .bind(replacement.channel_ref.channel().as_str())
        .bind(route_keys.external_tenant_key)
        .bind(route_keys.external_conversation_key)
        .bind(route_keys.external_thread_key)
        .bind(route)
        .bind(replacement.reason)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected()
            == 1;
        if !inserted {
            return Ok(false);
        }

        let affected = sqlx::query(&format!(
            "UPDATE {sessions} \
             SET channel = $1, active_channel_binding_id = $2, updated_at = NOW() \
             WHERE id = $3 AND tenant_id = $4"
        ))
        .bind(replacement.channel_ref.channel().as_str())
        .bind(binding_id.0)
        .bind(replacement.session_id.0)
        .bind(replacement.tenant_id.0)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if affected == 0 {
            return Err(MoaError::SessionNotFound(replacement.session_id));
        }

        Ok(true)
    }

    /// Loads the currently active channel binding route for a session, when one exists.
    pub async fn get_active_session_channel_binding(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Option<SessionChannelBinding>> {
        let sessions = self.table_name("sessions");
        let bindings = self.table_name("session_channel_bindings");
        let row = sqlx::query(&format!(
            "SELECT b.id, b.route \
             FROM {sessions} s \
             JOIN {bindings} b ON b.id = s.active_channel_binding_id \
             WHERE s.id = $1 AND b.ended_at IS NULL"
        ))
        .bind(session_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        let Some(row) = row else {
            return Ok(None);
        };
        let binding_id = row
            .col::<Uuid>("id")
            .map(moa_core::types::channel::SessionChannelBindingId)?;
        let channel_ref = row.col::<Json<ChannelRef>>("route").map(|route| route.0)?;
        Ok(Some(SessionChannelBinding {
            binding_id,
            channel_ref,
        }))
    }

    /// Resolves the active session bound to a channel route, when one exists.
    pub async fn get_active_session_binding_for_channel(
        &self,
        channel_ref: &ChannelRef,
    ) -> Result<Option<moa_core::types::channel::SessionChannelBindingResolution>> {
        let route_keys = channel_route_keys(channel_ref);
        let sessions = self.table_name("sessions");
        let bindings = self.table_name("session_channel_bindings");
        let rows = sqlx::query(&format!(
            "SELECT b.tenant_id, b.session_id, b.contact_id, b.id, b.route \
             FROM {bindings} b \
             JOIN {sessions} s ON s.id = b.session_id AND s.active_channel_binding_id = b.id \
             WHERE b.channel = $1 \
               AND b.ended_at IS NULL \
               AND b.external_conversation_key IS NOT NULL \
               AND COALESCE(b.external_tenant_key, '') = COALESCE($2, '') \
               AND COALESCE(b.external_conversation_key, '') = COALESCE($3, '') \
               AND COALESCE(b.external_thread_key, '') = COALESCE($4, '') \
             ORDER BY b.last_used_at DESC, b.created_at DESC \
             LIMIT 2"
        ))
        .bind(channel_ref.channel().as_str())
        .bind(route_keys.external_tenant_key)
        .bind(route_keys.external_conversation_key)
        .bind(route_keys.external_thread_key)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        if rows.len() > 1 {
            return Err(MoaError::ValidationError(
                "channel route is active in multiple tenants".to_string(),
            ));
        }
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let tenant_id = row
            .col::<Uuid>("tenant_id")
            .map(moa_core::types::identifiers::TenantId)?;
        let session_id = row.col::<Uuid>("session_id").map(SessionId)?;
        let contact_id = row
            .col::<Uuid>("contact_id")
            .map(moa_core::types::contact::ContactId)?;
        let binding_id = row
            .col::<Uuid>("id")
            .map(moa_core::types::channel::SessionChannelBindingId)?;
        let channel_ref = row.col::<Json<ChannelRef>>("route").map(|route| route.0)?;
        Ok(Some(
            moa_core::types::channel::SessionChannelBindingResolution {
                tenant_id,
                session_id,
                contact_id,
                binding: SessionChannelBinding {
                    binding_id,
                    channel_ref,
                },
            },
        ))
    }
}

#[async_trait]
impl SessionChannelStore for PostgresSessionStore {
    async fn replace_session_channel_binding(
        &self,
        update: SessionChannelBindingUpdate,
    ) -> Result<SessionChannelBindingId> {
        PostgresSessionStore::replace_session_channel_binding(
            self,
            SessionChannelBindingReplacement {
                tenant_id: update.tenant_id,
                storage_partition_id: &update.storage_partition_id,
                session_id: update.session_id,
                contact_id: update.contact_id,
                channel_account_id: update.channel_account_id,
                contact_point_id: update.contact_point_id,
                channel_ref: &update.channel_ref,
                reason: update.reason.as_deref(),
            },
        )
        .await
    }

    async fn get_active_session_channel_binding(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Option<SessionChannelBinding>> {
        PostgresSessionStore::get_active_session_channel_binding(self, session_id).await
    }

    async fn get_active_session_binding_for_channel(
        &self,
        channel_ref: &ChannelRef,
    ) -> Result<Option<moa_core::types::channel::SessionChannelBindingResolution>> {
        PostgresSessionStore::get_active_session_binding_for_channel(self, channel_ref).await
    }
}

struct ChannelRouteKeys {
    external_tenant_key: Option<String>,
    external_conversation_key: Option<String>,
    external_thread_key: Option<String>,
}

fn channel_route_keys(channel_ref: &moa_core::types::channel::ChannelRef) -> ChannelRouteKeys {
    match channel_ref {
        moa_core::types::channel::ChannelRef::Chat {
            conversation_id,
            client_session_id,
            ..
        } => ChannelRouteKeys {
            external_tenant_key: None,
            external_conversation_key: Some(conversation_id.clone()),
            external_thread_key: client_session_id.clone(),
        },
        moa_core::types::channel::ChannelRef::Slack {
            team_id,
            slack_channel_id,
            thread_ts,
            user_id,
        } => ChannelRouteKeys {
            external_tenant_key: team_id.clone(),
            external_conversation_key: slack_channel_id.clone().or_else(|| user_id.clone()),
            external_thread_key: thread_ts.clone(),
        },
        moa_core::types::channel::ChannelRef::Email { channel_account_id }
        | moa_core::types::channel::ChannelRef::Sms { channel_account_id } => ChannelRouteKeys {
            external_tenant_key: None,
            external_conversation_key: Some(channel_account_id.to_string()),
            external_thread_key: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{types::channel::ChannelAccountId, types::channel::ChannelRef};

    use super::channel_route_keys;

    #[test]
    fn channel_route_keys_use_slack_conversation_and_thread_columns() {
        // Pins: inbound Slack lookup remains indexable without JSONB predicates.
        let keys = channel_route_keys(&ChannelRef::Slack {
            team_id: Some("T123".to_string()),
            slack_channel_id: Some("C123".to_string()),
            thread_ts: Some("1700000000.000100".to_string()),
            user_id: Some("U123".to_string()),
        });

        assert_eq!(keys.external_tenant_key.as_deref(), Some("T123"));
        assert_eq!(keys.external_conversation_key.as_deref(), Some("C123"));
        assert_eq!(
            keys.external_thread_key.as_deref(),
            Some("1700000000.000100")
        );
    }

    #[test]
    fn channel_route_keys_use_account_id_for_email_and_sms() {
        // Pins: email/SMS inbound lookup keys do not duplicate raw addresses or phone numbers.
        let account_id = ChannelAccountId::new();
        let account_id_text = account_id.to_string();
        let keys = channel_route_keys(&ChannelRef::Email {
            channel_account_id: account_id,
        });

        assert_eq!(keys.external_tenant_key, None);
        assert_eq!(
            keys.external_conversation_key.as_deref(),
            Some(account_id_text.as_str())
        );
        assert_eq!(keys.external_thread_key, None);
    }
}
