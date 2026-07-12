//! Row mapping helpers for session query results.

use super::*;

/// Decodes individual row columns with consistent sqlx error mapping.
pub(crate) trait RowExt {
    /// Decodes a single column by name, mapping sqlx errors via `map_sqlx_error`.
    fn col<'r, T>(&'r self, name: &str) -> Result<T>
    where
        T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>;
}

impl RowExt for PgRow {
    fn col<'r, T>(&'r self, name: &str) -> Result<T>
    where
        T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
    {
        self.try_get::<T, _>(name).map_err(map_sqlx_error)
    }
}

pub(crate) fn session_meta_from_row(row: &PgRow) -> Result<SessionMeta> {
    let id = row.col::<Uuid>("id")?;
    let tenant_id = row.col::<Uuid>("tenant_id")?;
    let status_text = row.col::<String>("status")?;
    let channel_text = row.col::<String>("channel")?;
    let model = row.col::<String>("model")?;

    let contact_id = row.col::<Option<Uuid>>("contact_id")?;
    let contact_tenant_id = row.col::<Option<Uuid>>("contact_tenant_id")?;
    let contact_state = row.col::<Option<String>>("contact_state")?;
    let contact_canonical_id = row.col::<Option<Uuid>>("contact_canonical_id")?;
    let contact_linked_ids = row.col::<Vec<Uuid>>("contact_linked_ids")?;
    let contact_scopes = row.col::<Vec<String>>("contact_scopes")?;
    let created_by_actor_type = row.col::<Option<String>>("created_by_actor_type")?;
    let created_by_actor_id = row.col::<Option<Uuid>>("created_by_actor_id")?;

    Ok(SessionMeta {
        id: moa_core::types::identifiers::SessionId(id),
        tenant_id: TenantId(tenant_id),
        title: row.col::<Option<String>>("title")?,
        status: from_db("session status", &status_text)?,
        channel: from_db("channel", &channel_text)?,
        active_channel_binding_id: row
            .col::<Option<Uuid>>("active_channel_binding_id")?
            .map(SessionChannelBindingId),
        model: ModelId::new(model),
        created_at: row.col::<DateTime<Utc>>("created_at")?,
        updated_at: row.col::<DateTime<Utc>>("updated_at")?,
        completed_at: row.col::<Option<DateTime<Utc>>>("completed_at")?,
        parent_session_id: row
            .col::<Option<Uuid>>("parent_session_id")?
            .map(moa_core::types::identifiers::SessionId),
        contact: contact_from_columns(
            contact_id,
            contact_tenant_id,
            contact_state.as_deref(),
            contact_canonical_id,
            contact_linked_ids,
            contact_scopes,
        )?,
        created_by: actor_from_columns(created_by_actor_type.as_deref(), created_by_actor_id)?,
        contact_promoted_from_id: row
            .col::<Option<Uuid>>("contact_promoted_from_id")?
            .map(ContactId),
        agent_context: None,
        total_input_tokens: row.col::<i64>("total_input_tokens")? as usize,
        total_input_tokens_uncached: row.col::<i64>("total_input_tokens_uncached")? as usize,
        total_input_tokens_cache_write: row.col::<i64>("total_input_tokens_cache_write")? as usize,
        total_input_tokens_cache_read: row.col::<i64>("total_input_tokens_cache_read")? as usize,
        total_output_tokens: row.col::<i64>("total_output_tokens")? as usize,
        total_cost_cents: row.col::<i64>("total_cost_cents")? as u32,
        event_count: row.col::<i64>("event_count")? as usize,
        last_checkpoint_seq: row
            .col::<Option<i64>>("last_checkpoint_seq")?
            .map(|value| value as u64),
    })
}

fn contact_from_columns(
    contact_id: Option<Uuid>,
    tenant_id: Option<Uuid>,
    state: Option<&str>,
    canonical_contact_id: Option<Uuid>,
    linked_contact_ids: Vec<Uuid>,
    scopes: Vec<String>,
) -> Result<Option<ContactRef>> {
    let Some(contact_id) = contact_id else {
        return Ok(None);
    };
    let tenant_id = tenant_id.ok_or_else(|| {
        MoaError::StorageError("session contact missing contact_tenant_id".to_string())
    })?;
    let state = match state {
        Some(value) => from_db::<ContactVerificationState>("contact state", value)?,
        None => ContactVerificationState::Anonymous,
    };
    Ok(Some(ContactRef {
        contact_id: ContactId(contact_id),
        tenant_id: TenantId(tenant_id),
        state,
        canonical_contact_id: canonical_contact_id.map(ContactId),
        linked_contact_ids: linked_contact_ids.into_iter().map(ContactId).collect(),
        scopes,
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }))
}

fn actor_from_columns(
    actor_type: Option<&str>,
    actor_id: Option<Uuid>,
) -> Result<Option<SessionActorRef>> {
    match (actor_type, actor_id) {
        (None, _) => Ok(None),
        (Some("anonymous"), _) => Ok(Some(SessionActorRef::Anonymous)),
        (Some("identity"), Some(id)) => Ok(Some(SessionActorRef::Identity { id })),
        (Some("contact"), Some(id)) => Ok(Some(SessionActorRef::Contact { id: ContactId(id) })),
        (Some(value), _) => Err(MoaError::StorageError(format!(
            "invalid session actor columns `{value}`"
        ))),
    }
}

/// Maps a `sessions` row into a `SessionSummary`.
pub(crate) fn session_summary_from_row(row: &PgRow) -> Result<SessionSummary> {
    let tenant_id = row.col::<Uuid>("tenant_id")?;
    let contact_id = row.col::<Option<Uuid>>("contact_id")?;
    let contact_tenant_id = row.col::<Option<Uuid>>("contact_tenant_id")?;
    let contact_state = row.col::<Option<String>>("contact_state")?;
    let contact_canonical_id = row.col::<Option<Uuid>>("contact_canonical_id")?;
    let contact_linked_ids = row.col::<Vec<Uuid>>("contact_linked_ids")?;
    let contact_scopes = row.col::<Vec<String>>("contact_scopes")?;
    let created_by_actor_type = row.col::<Option<String>>("created_by_actor_type")?;
    let created_by_actor_id = row.col::<Option<Uuid>>("created_by_actor_id")?;

    Ok(SessionSummary {
        session_id: moa_core::types::identifiers::SessionId(row.col::<Uuid>("id")?),
        tenant_id: TenantId(tenant_id),
        contact: contact_from_columns(
            contact_id,
            contact_tenant_id,
            contact_state.as_deref(),
            contact_canonical_id,
            contact_linked_ids,
            contact_scopes,
        )?,
        created_by: actor_from_columns(created_by_actor_type.as_deref(), created_by_actor_id)?,
        title: row.col::<Option<String>>("title")?,
        status: from_db("session status", &row.col::<String>("status")?)?,
        channel: from_db("channel", &row.col::<String>("channel")?)?,
        model: ModelId::new(row.col::<String>("model")?),
        updated_at: row.col::<DateTime<Utc>>("updated_at")?,
    })
}

fn tenant_id_from_storage(value: String) -> TenantId {
    if let Ok(uuid) = Uuid::parse_str(&value) {
        return TenantId::from(uuid);
    }
    let digest = Sha256::digest(value.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    TenantId::from(Uuid::from_bytes(bytes))
}

fn action_rule_scope_from_columns(
    scope: &str,
    tenant_id: Uuid,
    user_id: Option<String>,
) -> Result<ActionRuleScope> {
    match (scope, user_id) {
        ("tenant", None) => Ok(ActionRuleScope::Tenant {
            tenant_id: TenantId(tenant_id),
        }),
        ("contact", Some(user_id)) => {
            let contact_id = Uuid::parse_str(&user_id)
                .map(moa_core::types::contact::ContactId)
                .map_err(|error| {
                    MoaError::StorageError(format!(
                        "invalid action policy contact scope user_id `{user_id}`: {error}"
                    ))
                })?;
            Ok(ActionRuleScope::Contact {
                tenant_id: TenantId(tenant_id),
                contact_id,
            })
        }
        _ => Err(MoaError::StorageError(format!(
            "unsupported action policy scope columns for `{scope}`"
        ))),
    }
}

/// Maps a `task_segments` row into a `TaskSegment`.
pub(crate) fn task_segment_from_row(row: &PgRow) -> Result<TaskSegment> {
    Ok(TaskSegment {
        id: SegmentId(row.col::<Uuid>("id")?),
        session_id: SessionId(row.col::<Uuid>("session_id")?),
        tenant_id: row.col::<String>("tenant_id")?,
        segment_index: row.col::<i32>("segment_index")? as u32,
        task_summary: row.col::<Option<String>>("task_summary")?,
        started_at: row.col::<DateTime<Utc>>("started_at")?,
        ended_at: row.col::<Option<DateTime<Utc>>>("ended_at")?,
        outcome: row.col::<Option<String>>("outcome")?,
        assessment: parse_segment_assessment(row.col::<Option<String>>("assessment")?)?,
        outcome_confidence: row.col::<Option<f64>>("outcome_confidence")?,
        tools_used: row.col::<Vec<String>>("tools_used")?,
        skills_activated: row.col::<Vec<String>>("skills_activated")?,
        skills_used: row.col::<Vec<String>>("skills_used")?,
        turn_count: row.col::<i32>("turn_count")? as u32,
        token_cost: row.col::<i64>("token_cost")? as u64,
        previous_segment_id: row
            .col::<Option<Uuid>>("previous_segment_id")?
            .map(SegmentId),
    })
}

/// Maps a `learning_log` row into a `LearningEntry`.
pub(crate) fn learning_entry_from_row(row: &PgRow) -> Result<LearningEntry> {
    Ok(LearningEntry {
        id: row.col::<Uuid>("id")?,
        tenant_id: tenant_id_from_storage(row.col::<String>("tenant_id")?),
        learning_type: row.col::<String>("learning_type")?,
        target_id: row.col::<String>("target_id")?,
        target_label: row.col::<Option<String>>("target_label")?,
        payload: row.col::<serde_json::Value>("payload")?,
        confidence: row.col::<Option<f64>>("confidence")?,
        source_refs: row.col::<Vec<Uuid>>("source_refs")?,
        actor: row.col::<String>("actor")?,
        valid_from: row.col::<DateTime<Utc>>("valid_from")?,
        valid_to: row.col::<Option<DateTime<Utc>>>("valid_to")?,
        batch_id: row.col::<Option<Uuid>>("batch_id")?,
        version: row.col::<i32>("version")?,
    })
}

/// Maps an `experience_records` row into an `ExperienceRecord`.
pub(crate) fn experience_record_from_row(row: &PgRow) -> Result<ExperienceRecord> {
    let user_id = row
        .col::<Option<String>>("user_id")?
        .ok_or_else(|| MoaError::StorageError("experience record missing user_id".to_string()))?;
    Ok(ExperienceRecord {
        id: row.col::<Uuid>("id")?,
        segment_id: SegmentId(row.col::<Uuid>("segment_id")?),
        session_id: SessionId(row.col::<Uuid>("session_id")?),
        tenant_id: tenant_id_from_storage(row.col::<String>("tenant_id")?),
        user_id: UserId(user_id),
        task_summary: row.col::<Option<String>>("task_summary")?,
        task_fingerprint: json_column(row, "task_fingerprint_payload")?,
        task_facets: json_column(row, "task_facets")?,
        actions: row.col::<Vec<String>>("actions")?,
        resources: json_column(row, "resources")?,
        outcome: from_db("segment outcome", &row.col::<String>("outcome")?)?,
        confidence: row.col::<f64>("confidence")?,
        evidence: json_column(row, "evidence")?,
        tools_used: row.col::<Vec<String>>("tools_used")?,
        skills_activated: row.col::<Vec<String>>("skills_activated")?,
        skills_used: row.col::<Vec<String>>("skills_used")?,
        turn_count: row.col::<i32>("turn_count")? as u32,
        token_cost: row.col::<i64>("token_cost")? as u64,
        duration_ms: row
            .col::<Option<i64>>("duration_ms")?
            .map(|value| value as u64),
        assessment_policy_version: row.col::<String>("assessment_policy_version")?,
        extraction_policy_version: row.col::<String>("extraction_policy_version")?,
        created_at: row.col::<DateTime<Utc>>("created_at")?,
    })
}

/// Maps an `experience_attributions` row into an `ExperienceAttribution`.
pub(crate) fn experience_attribution_from_row(row: &PgRow) -> Result<ExperienceAttribution> {
    Ok(ExperienceAttribution {
        id: row.col::<Uuid>("id")?,
        experience_id: row.col::<Uuid>("experience_id")?,
        tenant_id: tenant_id_from_storage(row.col::<String>("tenant_id")?),
        user_id: row.col::<Option<String>>("user_id")?.map(UserId),
        subject_type: from_db(
            "attribution subject type",
            &row.col::<String>("subject_type")?,
        )?,
        subject_id: row.col::<String>("subject_id")?,
        effect: from_db("attribution effect", &row.col::<String>("effect")?)?,
        kind: from_db("attribution kind", &row.col::<String>("kind")?)?,
        confidence: row.col::<f64>("confidence")?,
        evidence: json_column(row, "evidence")?,
        created_at: row.col::<DateTime<Utc>>("created_at")?,
    })
}

/// Maps a `learning_candidates` row into a `LearningCandidate`.
pub(crate) fn learning_candidate_from_row(row: &PgRow) -> Result<LearningCandidate> {
    Ok(LearningCandidate {
        id: row.col::<Uuid>("id")?,
        tenant_id: tenant_id_from_storage(row.col::<String>("tenant_id")?),
        user_id: row.col::<Option<String>>("user_id")?.map(UserId),
        candidate_type: from_db(
            "learning candidate type",
            &row.col::<String>("candidate_type")?,
        )?,
        status: from_db("learning candidate status", &row.col::<String>("status")?)?,
        target_id: row.col::<Option<String>>("target_id")?,
        target_label: row.col::<Option<String>>("target_label")?,
        task_fingerprint: row
            .col::<Option<serde_json::Value>>("task_fingerprint_payload")?
            .map(|value| {
                serde_json::from_value::<TaskFingerprint>(value).map_err(|error| {
                    MoaError::StorageError(format!("invalid task fingerprint payload: {error}"))
                })
            })
            .transpose()?,
        task_facets: row
            .col::<Option<serde_json::Value>>("task_facets")?
            .map(|value| {
                serde_json::from_value(value).map_err(|error| {
                    MoaError::StorageError(format!("invalid task facet payload: {error}"))
                })
            })
            .transpose()?,
        payload: row.col::<serde_json::Value>("payload")?,
        evaluation_payload: row.col::<Option<serde_json::Value>>("evaluation_payload")?,
        source_experience_ids: row.col::<Vec<Uuid>>("source_experience_ids")?,
        confidence: row.col::<Option<f64>>("confidence")?,
        risk_class: from_db("learning risk class", &row.col::<String>("risk_class")?)?,
        promotion_requirements: row.col::<Vec<String>>("promotion_requirements")?,
        status_reason: row.col::<Option<String>>("status_reason")?,
        batch_id: row.col::<Option<Uuid>>("batch_id")?,
        created_at: row.col::<DateTime<Utc>>("created_at")?,
        updated_at: row.col::<DateTime<Utc>>("updated_at")?,
    })
}

/// Maps a `task_strategy_success_rates` row into a task-conditioned aggregate.
pub(crate) fn task_strategy_success_rate_from_row(row: &PgRow) -> Result<TaskStrategySuccessRate> {
    Ok(TaskStrategySuccessRate {
        tenant_id: tenant_id_from_storage(row.col::<String>("tenant_id")?),
        task_fingerprint: row.col::<String>("task_fingerprint")?,
        subject_type: from_db(
            "attribution subject type",
            &row.col::<String>("subject_type")?,
        )?,
        subject_id: row.col::<String>("subject_id")?,
        uses: row.col::<i64>("uses")? as u64,
        success_rate: row.col::<f64>("success_rate")?,
        avg_confidence: row.col::<f64>("avg_confidence")?,
        avg_token_cost: row.col::<f64>("avg_token_cost")?,
        avg_turn_count: row.col::<f64>("avg_turn_count")?,
        effect_score: row.col::<f64>("effect_score")?,
        unused_injections: row.col::<i64>("unused_injections")? as u64,
    })
}

fn json_column<T>(row: &PgRow, column: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let value = row.col::<serde_json::Value>(column)?;
    serde_json::from_value(value)
        .map_err(|error| MoaError::StorageError(format!("invalid {column} payload: {error}")))
}

fn parse_segment_assessment(value: Option<String>) -> Result<Option<SegmentAssessment>> {
    value
        .map(|value| {
            serde_json::from_str::<SegmentAssessment>(&value).map_err(|error| {
                MoaError::StorageError(format!("invalid segment assessment payload: {error}"))
            })
        })
        .transpose()
}

/// Maps an `action_policy_rules` row into an `ActionPolicyRule`.
pub(crate) fn action_policy_rule_from_row(row: &PgRow) -> Result<ActionPolicyRule> {
    let tenant_id = row.col::<Uuid>("tenant_id")?;
    let scope = row.col::<String>("scope")?;
    let user_id = row.col::<Option<String>>("user_id")?;
    Ok(ActionPolicyRule {
        id: row.col::<Uuid>("id")?,
        scope: action_rule_scope_from_columns(&scope, tenant_id, user_id)?,
        tool: row.col::<String>("tool")?,
        pattern: row.col::<String>("pattern")?,
        effect: from_db("action policy effect", &row.col::<String>("effect")?)?,
        reason: row.col::<Option<String>>("reason")?,
        created_by: moa_core::types::identifiers::UserId(row.col::<String>("created_by")?),
        created_at: row.col::<DateTime<Utc>>("created_at")?,
    })
}
