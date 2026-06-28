//! `moa_core::SessionStore` implementation for `PostgresSessionStore`.

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

impl PostgresSessionStore {
    /// Insert a session metadata row using a caller-owned transaction.
    ///
    /// This lets higher-level handlers atomically persist the session and its
    /// authorization outbox tuples. The caller owns commit/rollback and should
    /// call [`PostgresSessionStore::refresh_active_session_metric`] after a
    /// successful commit.
    pub async fn create_session_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        meta: SessionMeta,
    ) -> Result<moa_core::SessionId> {
        validate_session_create_meta(&meta)?;
        let session_id = meta.id;
        let tenant_id = meta.tenant_id;
        let tenant_storage_key = StoragePartitionId::for_tenant(tenant_id);
        let actor_storage_key = session_actor_storage_key(meta.created_by.as_ref());
        let status = meta.status.clone();
        let agent_context = meta.agent_context.clone();
        let sessions = self.table_name("sessions");
        sqlx::query(&format!(
            "INSERT INTO {sessions} ({SESSION_INSERT_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30)"
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

        Ok(session_id)
    }

    async fn insert_session_agent_context_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        session_id: moa_core::SessionId,
        tenant_id: moa_core::TenantId,
        actor_storage_key: &str,
        context: &moa_core::AgentContext,
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

    /// Replaces a session's active channel binding and current channel metadata.
    pub async fn replace_session_channel_binding(
        &self,
        replacement: SessionChannelBindingReplacement<'_>,
    ) -> Result<moa_core::SessionChannelBindingId> {
        let binding_id = moa_core::SessionChannelBindingId::new();
        let route = serde_json::to_value(replacement.channel_ref)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        let route_keys = channel_route_keys(replacement.channel_ref);
        let sessions = self.table_name("sessions");
        let bindings = self.table_name("session_channel_bindings");
        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;

        sqlx::query(&format!(
            "UPDATE {bindings} \
             SET ended_at = NOW(), last_used_at = NOW() \
             WHERE session_id = $1 AND ended_at IS NULL"
        ))
        .bind(replacement.session_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        sqlx::query(&format!(
            "INSERT INTO {bindings} \
                 (id, tenant_id, storage_partition_id, session_id, contact_id, channel_account_id, \
                  contact_point_id, channel, external_tenant_key, external_conversation_key, \
                  external_thread_key, route, reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"
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
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let affected = sqlx::query(&format!(
            "UPDATE {sessions} \
             SET channel = $1, active_channel_binding_id = $2, updated_at = NOW() \
             WHERE id = $3 AND tenant_id = $4"
        ))
        .bind(replacement.channel_ref.channel().as_str())
        .bind(binding_id.0)
        .bind(replacement.session_id.0)
        .bind(replacement.tenant_id.0)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        if affected == 0 {
            return Err(MoaError::SessionNotFound(replacement.session_id));
        }

        tx.commit().await.map_err(map_sqlx_error)?;
        Ok(binding_id)
    }

    /// Loads the currently active channel binding route for a session, when one exists.
    pub async fn get_active_session_channel_binding(
        &self,
        session_id: moa_core::SessionId,
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
            .try_get::<Uuid, _>("id")
            .map(moa_core::SessionChannelBindingId)
            .map_err(map_sqlx_error)?;
        let channel_ref = row
            .try_get::<Json<ChannelRef>, _>("route")
            .map(|route| route.0)
            .map_err(map_sqlx_error)?;
        Ok(Some(SessionChannelBinding {
            binding_id,
            channel_ref,
        }))
    }

    /// Updates contact metadata attached to an existing session.
    pub async fn update_session_contact(
        &self,
        session_id: moa_core::SessionId,
        contact: moa_core::ContactRef,
        promoted_from: Option<moa_core::ContactId>,
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

    /// Returns whether a persisted tool event exists without decoding matching payloads.
    pub async fn tool_event_exists(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::SessionId,
        event_type: EventType,
        tool_call_id: ToolCallId,
    ) -> Result<bool> {
        let events = self.table_name("events");
        sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM {events} \
                 WHERE storage_partition_id = $1 \
                   AND event_type = $2 \
                   AND payload -> 'data' ? 'tool_id' \
                   AND payload -> 'data' ->> 'tool_id' = $3 \
                   AND session_id = $4\
             )"
        ))
        .bind(storage_partition_id.as_str())
        .bind(event_type.as_str())
        .bind(tool_call_id.to_string())
        .bind(session_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)
    }

    /// Returns whether a persisted action-review event exists without decoding matching payloads.
    pub async fn action_review_event_exists(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::SessionId,
        event_type: EventType,
        review_id: Uuid,
    ) -> Result<bool> {
        let events = self.table_name("events");
        sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM {events} \
                 WHERE storage_partition_id = $1 \
                   AND event_type = $2 \
                   AND payload -> 'data' ? 'review_id' \
                   AND payload -> 'data' ->> 'review_id' = $3 \
                   AND session_id = $4\
             )"
        ))
        .bind(storage_partition_id.as_str())
        .bind(event_type.as_str())
        .bind(review_id.to_string())
        .bind(session_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)
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
        session_id: moa_core::SessionId,
    ) -> Result<Option<SessionChannelBinding>> {
        PostgresSessionStore::get_active_session_channel_binding(self, session_id).await
    }
}

#[async_trait]
impl SessionEventLookupStore for PostgresSessionStore {
    async fn tool_event_exists(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::SessionId,
        event_type: EventType,
        tool_call_id: ToolCallId,
    ) -> Result<bool> {
        PostgresSessionStore::tool_event_exists(
            self,
            storage_partition_id,
            session_id,
            event_type,
            tool_call_id,
        )
        .await
    }

    async fn action_review_event_exists(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::SessionId,
        event_type: EventType,
        review_id: Uuid,
    ) -> Result<bool> {
        PostgresSessionStore::action_review_event_exists(
            self,
            storage_partition_id,
            session_id,
            event_type,
            review_id,
        )
        .await
    }
}

fn agent_context_from_row(row: &sqlx::postgres::PgRow) -> Result<Option<moa_core::AgentContext>> {
    let Some(revision_uid) = row
        .try_get::<Option<Uuid>, _>("agent_revision_uid")
        .map_err(map_sqlx_error)?
    else {
        return Ok(None);
    };

    let artifact_dependencies = row
        .try_get::<Json<Vec<moa_core::ResolvedArtifactRevisionRef>>, _>("artifact_dependencies")
        .map_err(map_sqlx_error)?
        .0;
    let tool_dependencies = row
        .try_get::<Json<Vec<moa_core::LockedToolRef>>, _>("tool_dependencies")
        .map_err(map_sqlx_error)?
        .0;

    Ok(Some(moa_core::AgentContext {
        agent_id: row
            .try_get::<Option<Uuid>, _>("agent_id")
            .map_err(map_sqlx_error)?,
        installation_uid: row
            .try_get::<Option<Uuid>, _>("installation_uid")
            .map_err(map_sqlx_error)?,
        deployment_uid: row
            .try_get::<Option<Uuid>, _>("deployment_uid")
            .map_err(map_sqlx_error)?,
        definition_ref: row
            .try_get::<String, _>("agent_definition_ref")
            .map_err(map_sqlx_error)?,
        revision_uid,
        policy_hash: row
            .try_get::<String, _>("policy_hash")
            .map_err(map_sqlx_error)?,
        display_name: row
            .try_get::<String, _>("display_name")
            .map_err(map_sqlx_error)?,
        artifact_dependencies,
        tool_dependencies,
        policy_snapshot: row
            .try_get::<serde_json::Value, _>("policy_snapshot")
            .map_err(map_sqlx_error)?,
    }))
}

struct ChannelRouteKeys {
    external_tenant_key: Option<String>,
    external_conversation_key: Option<String>,
    external_thread_key: Option<String>,
}

fn channel_route_keys(channel_ref: &moa_core::ChannelRef) -> ChannelRouteKeys {
    match channel_ref {
        moa_core::ChannelRef::Chat {
            conversation_id,
            client_session_id,
            ..
        } => ChannelRouteKeys {
            external_tenant_key: None,
            external_conversation_key: Some(conversation_id.clone()),
            external_thread_key: client_session_id.clone(),
        },
        moa_core::ChannelRef::Slack {
            team_id,
            slack_channel_id,
            thread_ts,
            user_id,
        } => ChannelRouteKeys {
            external_tenant_key: team_id.clone(),
            external_conversation_key: slack_channel_id.clone().or_else(|| user_id.clone()),
            external_thread_key: thread_ts.clone(),
        },
        moa_core::ChannelRef::Email { channel_account_id }
        | moa_core::ChannelRef::Sms { channel_account_id } => ChannelRouteKeys {
            external_tenant_key: None,
            external_conversation_key: Some(channel_account_id.to_string()),
            external_thread_key: None,
        },
    }
}

fn session_actor_type(actor: &moa_core::SessionActorRef) -> &'static str {
    match actor {
        moa_core::SessionActorRef::Identity { .. } => "identity",
        moa_core::SessionActorRef::Contact { .. } => "contact",
        moa_core::SessionActorRef::Anonymous => "anonymous",
    }
}

fn session_actor_id(actor: &moa_core::SessionActorRef) -> Option<Uuid> {
    match actor {
        moa_core::SessionActorRef::Identity { id } => Some(*id),
        moa_core::SessionActorRef::Contact { id } => Some(id.0),
        moa_core::SessionActorRef::Anonymous => None,
    }
}

fn session_actor_storage_key(actor: Option<&moa_core::SessionActorRef>) -> String {
    match actor {
        Some(moa_core::SessionActorRef::Identity { id }) => format!("identity:{id}"),
        Some(moa_core::SessionActorRef::Contact { id }) => format!("contact:{id}"),
        Some(moa_core::SessionActorRef::Anonymous) => "anonymous".to_string(),
        None => "system".to_string(),
    }
}

#[async_trait]
impl SessionStore for PostgresSessionStore {
    /// Creates a new session record.
    async fn create_session(&self, meta: SessionMeta) -> Result<moa_core::SessionId> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let session_id = self.create_session_in_tx(&mut transaction, meta).await?;
        transaction.commit().await.map_err(map_sqlx_error)?;
        self.refresh_active_session_metric().await?;

        Ok(session_id)
    }

    /// Appends an event to the session log.
    async fn emit_event(&self, session_id: moa_core::SessionId, event: Event) -> Result<u64> {
        Ok(self
            .emit_event_record(session_id, event)
            .await?
            .sequence_num)
    }

    /// Appends an event and returns the persisted event record.
    async fn emit_event_record(
        &self,
        session_id: moa_core::SessionId,
        event: Event,
    ) -> Result<EventRecord> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let event_id = Uuid::now_v7();
        let event_type = event.type_name();
        let event_type_record = event.event_type();
        let hand_id = event_hand_id(&event);
        let token_count = event.token_count();
        let payload = encode_event_for_storage(
            self.blob_store.as_ref(),
            &session_id,
            &event,
            self.blob_threshold_bytes,
        )
        .await?;
        let now = Utc::now();
        let sessions = self.table_name("sessions");
        let events = self.table_name("events");

        let locked_session = sqlx::query(&format!(
            "SELECT event_count, tenant_id, storage_partition_id, user_id, contact_id FROM {sessions} WHERE id = $1 FOR UPDATE"
        ))
        .bind(session_id.0)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?
        .ok_or(MoaError::SessionNotFound(session_id))?;
        let sequence_num = locked_session
            .try_get::<i64, _>("event_count")
            .map_err(map_sqlx_error)? as u64;
        let tenant_id = locked_session
            .try_get::<Uuid, _>("tenant_id")
            .map_err(map_sqlx_error)?;
        let storage_partition_id = locked_session
            .try_get::<String, _>("storage_partition_id")
            .map_err(map_sqlx_error)?;
        let actor_storage_key = locked_session
            .try_get::<String, _>("user_id")
            .map_err(map_sqlx_error)?;
        let contact_id = locked_session
            .try_get::<Option<Uuid>, _>("contact_id")
            .map_err(map_sqlx_error)?;

        sqlx::query(&format!(
            "INSERT INTO {events} \
             (id, session_id, tenant_id, contact_id, storage_partition_id, user_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"
        ))
        .bind(event_id)
        .bind(session_id.0)
        .bind(tenant_id)
        .bind(contact_id)
        .bind(storage_partition_id)
        .bind(actor_storage_key)
        .bind(sequence_num as i64)
        .bind(event_type)
        .bind(Json(payload))
        .bind(now)
        .bind(Option::<Uuid>::None)
        .bind(&hand_id)
        .bind(token_count as i32)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?;

        transaction.commit().await.map_err(map_sqlx_error)?;
        if let Event::BrainResponse {
            model, model_tier, ..
        } = &event
        {
            record_turn_completed(model, *model_tier);
        }
        record_session_event_append(event_type);
        Ok(EventRecord {
            id: event_id,
            session_id,
            sequence_num,
            event_type: event_type_record,
            event,
            timestamp: now,
            brain_id: None,
            hand_id,
            token_count: Some(token_count),
        })
    }

    async fn store_text_artifact(
        &self,
        session_id: moa_core::SessionId,
        text: &str,
    ) -> Result<ClaimCheck> {
        let blob_id = self.blob_store.store(&session_id, text.as_bytes()).await?;
        Ok(ClaimCheck {
            blob_id,
            size: text.len(),
            preview: preview_text(text),
        })
    }

    async fn load_text_artifact(
        &self,
        session_id: moa_core::SessionId,
        claim_check: &ClaimCheck,
    ) -> Result<String> {
        let bytes = self
            .blob_store
            .get(&session_id, &claim_check.blob_id)
            .await?;
        String::from_utf8(bytes).map_err(|error| {
            MoaError::StorageError(format!(
                "blob `{}` did not contain valid UTF-8: {error}",
                claim_check.blob_id
            ))
        })
    }

    /// Retrieves events for a session within a sequence and type range.
    async fn get_events(
        &self,
        session_id: moa_core::SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        if matches!(range.event_types, Some(ref types) if types.is_empty()) {
            return Ok(Vec::new());
        }
        let started_at = std::time::Instant::now();
        let events = self.table_name("events");

        let use_recent_order =
            range.limit.is_some() && range.from_seq.is_none() && range.to_seq.is_none();

        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {EVENT_COLUMNS} FROM {events} WHERE session_id = "
        ));
        query.push_bind(session_id.0);

        if let Some(from_seq) = range.from_seq {
            query.push(" AND sequence_num >= ");
            query.push_bind(from_seq as i64);
        }
        if let Some(to_seq) = range.to_seq {
            query.push(" AND sequence_num <= ");
            query.push_bind(to_seq as i64);
        }
        if let Some(event_types) = range.event_types {
            query.push(" AND event_type IN (");
            let mut separated = query.separated(", ");
            for event_type in event_types {
                separated.push_bind(event_type.as_str());
            }
            separated.push_unseparated(")");
        }

        if use_recent_order {
            query.push(" ORDER BY sequence_num DESC");
        } else {
            query.push(" ORDER BY sequence_num ASC");
        }
        if let Some(limit) = range.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        let decoded_bytes = rows.iter().try_fold(0_u64, |total, row| {
            let payload_bytes = row
                .try_get_raw("payload")
                .map_err(map_sqlx_error)?
                .as_bytes()
                .map_err(|error| {
                    MoaError::StorageError(format!("failed to read event payload bytes: {error}"))
                })?;
            Ok::<_, MoaError>(total + payload_bytes.len() as u64)
        })?;
        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            events.push(self.event_record_from_row(row).await?);
        }
        if use_recent_order {
            events.reverse();
        }
        record_session_event_load(events.len() as u64);
        record_session_event_decoded_bytes(decoded_bytes);
        record_session_event_replay(events.len(), decoded_bytes, started_at.elapsed());
        Ok(events)
    }

    /// Loads a persisted session metadata record.
    async fn get_session(&self, session_id: moa_core::SessionId) -> Result<SessionMeta> {
        let sessions = self.table_name("sessions");
        let agent_contexts = self.table_name("session_agent_context");
        let session_columns = SESSION_SELECT_COLUMNS
            .split(", ")
            .map(|column| format!("s.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            r#"
            SELECT {session_columns},
                   ac.agent_id, ac.installation_uid, ac.deployment_uid,
                   ac.agent_definition_ref, ac.agent_revision_uid, ac.policy_hash,
                   ac.display_name, ac.policy_snapshot, ac.artifact_dependencies,
                   ac.tool_dependencies
            FROM {sessions} AS s
            LEFT JOIN {agent_contexts} AS ac ON ac.session_id = s.id
            WHERE s.id = $1
            LIMIT 1
            "#
        );
        let row = sqlx::query(&query)
            .bind(session_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx_error)?
            .ok_or(MoaError::SessionNotFound(session_id))?;
        let mut meta = session_meta_from_row(&row)?;
        let agent_context = agent_context_from_row(&row)?.ok_or_else(|| {
            MoaError::StorageError(format!(
                "session {session_id} is missing required agent context"
            ))
        })?;
        meta.agent_context = Some(agent_context);
        Ok(meta)
    }

    /// Updates the status of an existing session.
    async fn update_status(
        &self,
        session_id: moa_core::SessionId,
        status: SessionStatus,
    ) -> Result<()> {
        let now = Utc::now();
        let sessions = self.table_name("sessions");
        let completed_at = if matches!(
            status,
            SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
        ) {
            Some(now)
        } else {
            None
        };

        let affected = sqlx::query(&format!(
            "UPDATE {sessions} SET status = $1, updated_at = $2, completed_at = $3 WHERE id = $4"
        ))
        .bind(status.as_str())
        .bind(now)
        .bind(completed_at)
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::SessionNotFound(session_id));
        }
        self.refresh_active_session_metric().await?;

        Ok(())
    }

    async fn update_session_contact(
        &self,
        session_id: moa_core::SessionId,
        contact: moa_core::ContactRef,
        promoted_from: Option<moa_core::ContactId>,
    ) -> Result<()> {
        PostgresSessionStore::update_session_contact(self, session_id, contact, promoted_from).await
    }

    /// Stores the latest context snapshot for a session.
    async fn put_snapshot(
        &self,
        session_id: moa_core::SessionId,
        snapshot: ContextSnapshot,
    ) -> Result<()> {
        let context_snapshots = self.table_name("context_snapshots");
        let sessions = self.table_name("sessions");
        let affected = sqlx::query(&format!(
            "INSERT INTO {context_snapshots} \
             (session_id, storage_partition_id, user_id, format_version, last_sequence_num, payload, created_at) \
             SELECT $1, s.storage_partition_id, s.user_id, $2, $3, $4, $5 \
             FROM {sessions} s WHERE s.id = $1 \
             ON CONFLICT (session_id) DO UPDATE SET \
                 storage_partition_id = EXCLUDED.storage_partition_id, \
                 user_id = EXCLUDED.user_id, \
                 format_version = EXCLUDED.format_version, \
                 last_sequence_num = EXCLUDED.last_sequence_num, \
                 payload = EXCLUDED.payload, \
                 created_at = EXCLUDED.created_at"
        ))
        .bind(session_id.0)
        .bind(snapshot.format_version as i32)
        .bind(snapshot.last_sequence_num as i64)
        .bind(Json(snapshot))
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::SessionNotFound(session_id));
        }
        Ok(())
    }

    /// Loads the latest context snapshot for a session when one exists.
    async fn get_snapshot(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Option<ContextSnapshot>> {
        let context_snapshots = self.table_name("context_snapshots");
        let row = sqlx::query(&format!(
            "SELECT payload FROM {context_snapshots} WHERE session_id = $1 LIMIT 1"
        ))
        .bind(session_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| {
            row.try_get::<Json<ContextSnapshot>, _>("payload")
                .map(|payload| payload.0)
                .map_err(map_sqlx_error)
        })
        .transpose()
    }

    /// Deletes the stored context snapshot for a session.
    async fn delete_snapshot(&self, session_id: moa_core::SessionId) -> Result<()> {
        let context_snapshots = self.table_name("context_snapshots");
        sqlx::query(&format!(
            "DELETE FROM {context_snapshots} WHERE session_id = $1"
        ))
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }

    /// Searches events using `PostgreSQL` full-text search and optional session filters.
    async fn search_events(
        &self,
        query_text: &str,
        filter: EventFilter,
    ) -> Result<Vec<EventRecord>> {
        let normalized_query = normalize_event_search_query(query_text);
        if normalized_query.is_empty() {
            return Ok(Vec::new());
        }
        if matches!(filter.event_types, Some(ref types) if types.is_empty()) {
            return Ok(Vec::new());
        }
        let events = self.table_name("events");
        let sessions = self.table_name("sessions");

        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT e.id, e.session_id, e.sequence_num, e.event_type, e.payload, \
             e.timestamp, e.brain_id, e.hand_id, e.token_count, \
             ts_rank(e.search_vector, plainto_tsquery('english', "
                .to_string(),
        );
        query.push_bind(normalized_query.clone());
        query.push(format!(
            ")) AS rank \
             FROM {events} e JOIN {sessions} s ON s.id = e.session_id \
             WHERE e.search_vector @@ plainto_tsquery('english', "
        ));
        query.push_bind(normalized_query);
        query.push(")");

        if let Some(session_id) = filter.session_id {
            query.push(" AND e.session_id = ");
            query.push_bind(session_id.0);
        }
        if let Some(tenant_id) = filter.tenant_id {
            query.push(" AND e.tenant_id = ");
            query.push_bind(tenant_id.0);
        }
        if let Some(contact_id) = filter.contact_id {
            query.push(" AND e.contact_id = ");
            query.push_bind(contact_id.0);
        }
        if let Some(from_time) = filter.from_time {
            query.push(" AND e.timestamp >= ");
            query.push_bind(from_time);
        }
        if let Some(to_time) = filter.to_time {
            query.push(" AND e.timestamp <= ");
            query.push_bind(to_time);
        }
        if let Some(event_types) = filter.event_types {
            query.push(" AND e.event_type IN (");
            let mut separated = query.separated(", ");
            for event_type in event_types {
                separated.push_bind(event_type.as_str());
            }
            separated.push_unseparated(")");
        }

        query.push(" ORDER BY rank DESC, e.timestamp DESC, e.sequence_num DESC");
        if let Some(limit) = filter.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            events.push(self.event_record_from_row(row).await?);
        }
        Ok(events)
    }

    /// Lists sessions filtered by tenant, contact, status, or channel.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        let sessions = self.table_name("sessions");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {SESSION_SUMMARY_COLUMNS} FROM {sessions} WHERE TRUE"
        ));

        if let Some(tenant_id) = filter.tenant_id {
            query.push(" AND tenant_id = ");
            query.push_bind(tenant_id.0);
        }
        if let Some(contact_id) = filter.contact_id {
            query.push(" AND contact_id = ");
            query.push_bind(contact_id.0);
        }
        if let Some(created_by) = filter.created_by {
            query.push(" AND created_by_actor_type = ");
            query.push_bind(session_actor_type(&created_by));
            match session_actor_id(&created_by) {
                Some(actor_id) => {
                    query.push(" AND created_by_actor_id = ");
                    query.push_bind(actor_id);
                }
                None => {
                    query.push(" AND created_by_actor_id IS NULL");
                }
            }
        }
        if let Some(status) = filter.status {
            query.push(" AND status = ");
            query.push_bind(status.as_str());
        }
        if let Some(channel) = filter.channel {
            query.push(" AND channel = ");
            query.push_bind(channel.as_str());
        }

        query.push(" ORDER BY updated_at DESC");
        if let Some(limit) = filter.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter().map(session_summary_from_row).collect()
    }

    /// Returns aggregate tenant spend in cents since the provided UTC timestamp.
    async fn tenant_cost_since(&self, tenant_id: &TenantId, since: DateTime<Utc>) -> Result<u32> {
        let events = self.table_name("events");
        let total = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COALESCE( \
                 SUM((e.payload -> 'data' ->> 'cost_cents')::BIGINT), \
                 0 \
             )::BIGINT \
             FROM {events} e \
             WHERE e.tenant_id = $1 \
               AND e.event_type = $2 \
               AND e.timestamp >= $3"
        ))
        .bind(tenant_id.0)
        .bind("BrainResponse")
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        u32::try_from(total)
            .map_err(|_| MoaError::StorageError("tenant spend exceeded u32 range".to_string()))
    }

    /// Deletes a session only when it has no append-only events.
    async fn delete_empty_session(&self, session_id: moa_core::SessionId) -> Result<()> {
        let events = self.table_name("events");
        let pending_signals = self.table_name("pending_signals");
        let context_snapshots = self.table_name("context_snapshots");
        let task_segments = self.table_name("task_segments");
        let session_attachments = self.table_name("session_attachments");
        let sessions = self.table_name("sessions");

        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let Some(tenant_uuid) = sqlx::query_scalar::<_, Uuid>(&format!(
            "SELECT tenant_id FROM {sessions} WHERE id = $1 FOR UPDATE"
        ))
        .bind(session_id.0)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?
        else {
            return Err(MoaError::SessionNotFound(session_id));
        };
        let tenant_id = TenantId(tenant_uuid);
        let event_count = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {events} WHERE session_id = $1"
        ))
        .bind(session_id.0)
        .fetch_one(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?;

        if event_count > 0 {
            return Err(MoaError::Unsupported(format!(
                "session `{session_id}` has {event_count} append-only event(s); use privacy erase or tombstoning instead"
            )));
        }

        for sql in [
            format!("DELETE FROM {pending_signals} WHERE session_id = $1"),
            format!("DELETE FROM {context_snapshots} WHERE session_id = $1"),
            format!("DELETE FROM {task_segments} WHERE session_id = $1"),
        ] {
            sqlx::query(&sql)
                .bind(session_id.0)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx_error)?;
        }

        let attachment_object_keys = sqlx::query(&format!(
            "DELETE FROM {session_attachments} \
             WHERE tenant_id = $1 AND session_id = $2 \
             RETURNING object_key"
        ))
        .bind(tenant_id.0)
        .bind(session_id.0)
        .fetch_all(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?
        .into_iter()
        .map(|row| {
            row.try_get::<String, _>("object_key")
                .map_err(map_sqlx_error)
        })
        .collect::<Result<Vec<_>>>()?;
        let deleted = sqlx::query(&format!("DELETE FROM {sessions} WHERE id = $1"))
            .bind(session_id.0)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?
            .rows_affected();
        if deleted == 0 {
            return Err(MoaError::SessionNotFound(session_id));
        }

        transaction.commit().await.map_err(map_sqlx_error)?;
        self.refresh_active_session_metric().await?;

        if let Err(err) = self.blob_store.delete_session(&session_id).await {
            tracing::warn!(%err, session_id = %session_id, "blob cleanup failed after empty session delete");
        }
        for object_key in attachment_object_keys {
            if let Err(err) = self.attachment_store.delete(&object_key).await {
                tracing::warn!(
                    %err,
                    session_id = %session_id,
                    object_key,
                    "attachment object cleanup failed after empty session delete"
                );
            }
        }

        Ok(())
    }
}

#[async_trait]
impl SegmentStore for PostgresSessionStore {
    async fn create_segment(&self, segment: &TaskSegment) -> Result<()> {
        PostgresSessionStore::create_segment(self, segment).await
    }

    async fn complete_segment(
        &self,
        segment_id: SegmentId,
        update: SegmentCompletion,
    ) -> Result<()> {
        PostgresSessionStore::complete_segment(self, segment_id, update).await
    }

    async fn get_active_segment(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Option<TaskSegment>> {
        PostgresSessionStore::get_active_segment(self, session_id).await
    }

    async fn list_segments(&self, session_id: moa_core::SessionId) -> Result<Vec<TaskSegment>> {
        PostgresSessionStore::list_segments(self, session_id).await
    }

    async fn update_segment_assessment(
        &self,
        segment_id: SegmentId,
        assessment: &SegmentAssessment,
    ) -> Result<()> {
        PostgresSessionStore::update_segment_assessment(self, segment_id, assessment).await
    }

    async fn get_segment_baseline(&self, tenant_id: &str) -> Result<Option<SegmentBaseline>> {
        PostgresSessionStore::get_segment_baseline(self, tenant_id).await
    }

    async fn list_skill_resolution_rates(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<SkillResolutionRate>> {
        PostgresSessionStore::list_skill_resolution_rates(self, tenant_id).await
    }

    async fn list_task_strategy_success_rates(
        &self,
        tenant_id: &str,
        task_fingerprint: &str,
    ) -> Result<Vec<TaskStrategySuccessRate>> {
        PostgresSessionStore::list_task_strategy_success_rates(self, tenant_id, task_fingerprint)
            .await
    }

    async fn refresh_segment_materialized_views(&self) -> Result<()> {
        PostgresSessionStore::refresh_segment_materialized_views(self).await
    }

    async fn record_active_segment_tool_use(
        &self,
        session_id: moa_core::SessionId,
        tool_name: &str,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_tool_use(self, session_id, tool_name).await
    }

    async fn record_active_segment_skill_activation(
        &self,
        session_id: moa_core::SessionId,
        skill_name: &str,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_skill_activation(self, session_id, skill_name)
            .await
    }

    async fn record_active_segment_turn_usage(
        &self,
        session_id: moa_core::SessionId,
        token_cost: u64,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_turn_usage(self, session_id, token_cost).await
    }
}

#[async_trait]
impl ExperienceStore for PostgresSessionStore {
    async fn append_experience_record(&self, experience: &ExperienceRecord) -> Result<()> {
        PostgresSessionStore::append_experience_record(self, experience).await
    }

    async fn get_experience_record(
        &self,
        session_id: moa_core::SessionId,
        experience_id: uuid::Uuid,
    ) -> Result<Option<ExperienceRecord>> {
        PostgresSessionStore::get_experience_record(self, session_id, experience_id).await
    }

    async fn list_experience_records(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Vec<ExperienceRecord>> {
        PostgresSessionStore::list_experience_records(self, session_id).await
    }

    async fn append_experience_attributions(
        &self,
        attributions: &[ExperienceAttribution],
    ) -> Result<()> {
        PostgresSessionStore::append_experience_attributions(self, attributions).await
    }

    async fn list_experience_attributions(
        &self,
        experience_id: uuid::Uuid,
    ) -> Result<Vec<ExperienceAttribution>> {
        PostgresSessionStore::list_experience_attributions(self, experience_id).await
    }
}

#[async_trait]
impl LearningCandidateStore for PostgresSessionStore {
    async fn append_learning_candidate(&self, candidate: &LearningCandidate) -> Result<()> {
        PostgresSessionStore::append_learning_candidate(self, candidate).await
    }

    async fn get_learning_candidate(
        &self,
        tenant_id: &TenantId,
        candidate_id: Uuid,
    ) -> Result<Option<LearningCandidate>> {
        PostgresSessionStore::get_learning_candidate(self, tenant_id, candidate_id).await
    }

    async fn list_learning_candidates(
        &self,
        tenant_id: &str,
        status: Option<LearningCandidateStatus>,
        limit: usize,
    ) -> Result<Vec<LearningCandidate>> {
        PostgresSessionStore::list_learning_candidates(self, tenant_id, status, limit).await
    }

    async fn update_learning_candidate_status(
        &self,
        update: &LearningCandidateStatusUpdate,
    ) -> Result<()> {
        PostgresSessionStore::update_learning_candidate_status(self, update).await
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{ChannelAccountId, ChannelRef};

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
