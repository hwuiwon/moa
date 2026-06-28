//! SQL repository helpers for contact identity state.

use chrono::{DateTime, Duration, Utc};
use moa_core::{
    Channel, ChannelAccountId, ChannelAccountRef, ChannelRef, ContactId, ContactPointId,
    ContactPointInput, ContactPointKind, ContactPointRef, ContactRef, ContactTokenClaims,
    ContactTokenIssueRequest, ContactVerificationChallengeId, ContactVerificationStartResponse,
    ContactVerificationState, MessagingConfig, MoaError, SessionMeta, StoragePartitionId, TenantId,
};
use moa_messaging::{DeliveryMessage, DeliverySink, ProviderDeliverySink};
use sqlx::Row;
use uuid::Uuid;

use crate::domain::{
    contact_allows_channel_contact, contact_point_delivery, hash_contact_point_with_key_hex,
    hash_verification_code, normalize_contact_point, parse_contact_point_kind, parse_contact_state,
};
use crate::{ContactError, Result};

const MAX_VERIFICATION_ATTEMPTS: i32 = 5;

/// Issues a contact row and any unverified contact points in one transaction.
pub async fn issue_contact(
    pool: sqlx::PgPool,
    contact_point_hash_key_hex: &str,
    tenant_id: TenantId,
    request: ContactTokenIssueRequest,
) -> Result<(ContactRef, Vec<ContactPointRef>)> {
    let contact_id = ContactId::new();
    let state = if request.contact_points.is_empty() {
        ContactVerificationState::Anonymous
    } else {
        ContactVerificationState::Unverified
    };
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| ContactError::database("begin contact issuance", error))?;
    sqlx::query(
        r#"
        INSERT INTO contacts (
            id, tenant_id, storage_partition_id, contact_id, state, display_name, profile, metadata
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
    .bind(contact_id.0)
    .bind(state.as_str())
    .bind(request.display_name.as_deref())
    .bind(&request.profile)
    .bind(&request.metadata)
    .execute(&mut *transaction)
    .await
    .map_err(|error| ContactError::database("insert contact", error))?;

    let mut contact_points = Vec::with_capacity(request.contact_points.len());
    for point in request.contact_points {
        let contact_point = insert_contact_point(
            &mut transaction,
            contact_point_hash_key_hex,
            tenant_id,
            contact_id,
            point,
            false,
        )
        .await?;
        contact_points.push(contact_point);
    }

    transaction
        .commit()
        .await
        .map_err(|error| ContactError::database("commit contact issuance", error))?;

    Ok((
        ContactRef {
            contact_id,
            tenant_id,
            state,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: request.permissions,
            agent_ids: request.agent_ids,
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        },
        contact_points,
    ))
}

/// Loads the persisted contact projection.
pub async fn load_contact_ref(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> Result<ContactRef> {
    let row = sqlx::query(
        r#"
        SELECT id, tenant_id, state, canonical_contact_id
        FROM contacts
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .fetch_optional(&pool)
    .await
    .map_err(|error| ContactError::database("load contact", error))?
    .ok_or_else(|| ContactError::terminal(404, "contact not found"))?;
    let state = row
        .try_get::<String, _>("state")
        .map_err(|error| ContactError::database("read contact state", error))?;
    Ok(ContactRef {
        contact_id,
        tenant_id,
        state: parse_contact_state(&state)?,
        canonical_contact_id: row
            .try_get::<Option<Uuid>, _>("canonical_contact_id")
            .map_err(|error| ContactError::database("read canonical contact id", error))?
            .map(ContactId),
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    })
}

/// Resolves contact-point inputs to existing verified contacts in one tenant.
///
/// Plaintext contact-point values are normalized and hashed locally, then only
/// the keyed hashes are sent to storage.
pub async fn resolve_verified_contact_ids(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    contact_point_hash_key_hex: &str,
    contact_points: &[ContactPointInput],
) -> Result<Vec<ContactId>> {
    let mut contact_ids = Vec::new();
    for point in contact_points {
        let normalized = normalize_contact_point(point.kind, &point.value)?;
        let normalized_hash = hash_contact_point_with_key_hex(
            tenant_id,
            point.kind,
            &normalized,
            contact_point_hash_key_hex,
        )?;
        let rows = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT DISTINCT contact_id
            FROM contact_points
            WHERE tenant_id = $1
              AND storage_partition_id = $2
              AND kind = $3
              AND normalized_hash = $4
              AND verified = TRUE
            ORDER BY contact_id
            "#,
        )
        .bind(tenant_id.0)
        .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
        .bind(point.kind.as_str())
        .bind(&normalized_hash)
        .fetch_all(pool)
        .await
        .map_err(|error| ContactError::database("resolve verified contact point", error))?;
        for row in rows {
            let contact_id = ContactId(row);
            if !contact_ids.contains(&contact_id) {
                contact_ids.push(contact_id);
            }
        }
    }
    Ok(contact_ids)
}

/// Persists a contact token grant for later revocation checks.
pub async fn create_contact_token_grant(
    pool: sqlx::PgPool,
    claims: &ContactTokenClaims,
    contact_id: ContactId,
    expires_at: DateTime<Utc>,
    issued_by_actor_type: &'static str,
    issued_by_actor_id: Option<Uuid>,
) -> Result<()> {
    let session_ids = claims
        .session_ids
        .iter()
        .map(|session_id| session_id.0)
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        INSERT INTO contact_token_grants
            (id, token_jti, tenant_id, storage_partition_id, contact_id, state, scopes, permissions,
             agent_ids, session_ids, issued_by_actor_type, issued_by_actor_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (token_jti) DO NOTHING
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(&claims.jti)
    .bind(claims.tenant_id.0)
    .bind(StoragePartitionId::for_tenant(claims.tenant_id).as_str())
    .bind(contact_id.0)
    .bind(claims.state.as_str())
    .bind(&claims.scopes)
    .bind(&claims.permissions)
    .bind(&claims.agent_ids)
    .bind(&session_ids)
    .bind(issued_by_actor_type)
    .bind(issued_by_actor_id)
    .bind(expires_at)
    .execute(&pool)
    .await
    .map_err(|error| ContactError::database("insert contact token grant", error))?;
    Ok(())
}

/// Verifies that a contact token grant is active and unexpired.
pub async fn ensure_contact_token_grant_active(
    pool: &sqlx::PgPool,
    claims: &ContactTokenClaims,
    contact_id: ContactId,
) -> Result<()> {
    let active = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM contact_token_grants
            WHERE token_jti = $1
              AND tenant_id = $2
              AND contact_id = $3
              AND state = $4
              AND revoked_at IS NULL
              AND expires_at > NOW()
        )
        "#,
    )
    .bind(&claims.jti)
    .bind(claims.tenant_id.0)
    .bind(contact_id.0)
    .bind(claims.state.as_str())
    .fetch_one(pool)
    .await
    .map_err(|error| ContactError::database("check contact token grant", error))?;
    if active {
        Ok(())
    } else {
        Err(ContactError::terminal(
            401,
            "contact token grant is not active",
        ))
    }
}

/// Command for creating and delivering a contact verification challenge.
#[derive(Debug, Clone)]
pub struct ContactVerificationStartCommand {
    /// Tenant/account boundary for the contact.
    pub tenant_id: TenantId,
    /// Contact that owns the verification attempt.
    pub contact_id: ContactId,
    /// Contact point to verify.
    pub contact_point: ContactPointInput,
    /// Optional caller-requested delivery channel.
    pub requested_channel: Option<Channel>,
    /// Challenge time-to-live in seconds.
    pub ttl_seconds: i64,
    /// Hex-encoded contact-point hash key.
    pub contact_point_hash_key_hex: String,
    /// Messaging provider configuration used for OTP delivery.
    pub messaging_config: MessagingConfig,
}

/// Channel route resolved for a contact-owned session.
#[derive(Debug, Clone)]
pub struct ResolvedSessionChannel {
    /// Canonical channel reference to store on the session.
    pub channel_ref: ChannelRef,
    /// Channel-account projection if this route is account-backed.
    pub channel_account: Option<ChannelAccountRef>,
    /// Verified contact point that backs this route, if any.
    pub contact_point_id: Option<ContactPointId>,
}

/// Creates and delivers an OTP verification challenge.
pub async fn start_contact_verification(
    pool: sqlx::PgPool,
    command: ContactVerificationStartCommand,
) -> Result<ContactVerificationStartResponse> {
    let delivery = contact_point_delivery(
        command.contact_point.kind,
        &command.contact_point.value,
        command.requested_channel,
    )?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| ContactError::database("begin contact verification", error))?;
    ensure_contact_in_tenant(&mut transaction, command.tenant_id, command.contact_id).await?;
    let contact_point = insert_contact_point(
        &mut transaction,
        &command.contact_point_hash_key_hex,
        command.tenant_id,
        command.contact_id,
        command.contact_point,
        false,
    )
    .await?;
    let challenge_id = ContactVerificationChallengeId::new();
    let code = crate::domain::verification_code();
    let expires_at = Utc::now() + Duration::seconds(command.ttl_seconds);
    sqlx::query(
        r#"
        UPDATE contact_verification_challenges
        SET consumed_at = NOW()
        WHERE contact_id = $1
          AND contact_point_id = $2
          AND tenant_id = $3
          AND consumed_at IS NULL
        "#,
    )
    .bind(command.contact_id.0)
    .bind(contact_point.id.0)
    .bind(command.tenant_id.0)
    .execute(&mut *transaction)
    .await
    .map_err(|error| {
        ContactError::database("close previous contact verification challenges", error)
    })?;
    sqlx::query(
        r#"
        INSERT INTO contact_verification_challenges
            (id, contact_id, contact_point_id, tenant_id, storage_partition_id, code_hash, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(challenge_id.0)
    .bind(command.contact_id.0)
    .bind(contact_point.id.0)
    .bind(command.tenant_id.0)
    .bind(StoragePartitionId::for_tenant(command.tenant_id).as_str())
    .bind(hash_verification_code(challenge_id, &code))
    .bind(expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(|error| ContactError::database("insert contact verification challenge", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| ContactError::database("commit contact verification", error))?;
    let sink = match ProviderDeliverySink::from_env(
        StoragePartitionId::for_tenant(command.tenant_id).as_str(),
        &command.messaging_config,
    )
    .await
    {
        Ok(sink) => sink,
        Err(error) => {
            if let Err(consume_error) =
                consume_contact_verification_challenge(&pool, challenge_id).await
            {
                tracing::warn!(
                    challenge_id = %challenge_id,
                    error = %consume_error,
                    "failed to consume undelivered contact verification challenge"
                );
            }
            return Err(contact_delivery_error(error));
        }
    };
    let delivery_message = DeliveryMessage::contact_verification_otp(
        command.tenant_id.0,
        command.contact_id,
        delivery.channel,
        delivery.destination,
        &code,
        expires_at,
    );
    match sink.deliver(delivery_message).await {
        Ok(receipt) => {
            tracing::info!(
                challenge_id = %challenge_id,
                contact_id = %command.contact_id,
                contact_point_id = %contact_point.id,
                delivery_channel = receipt.channel.as_str(),
                provider = %receipt.provider,
                provider_message_id = ?receipt.provider_message_id,
                provider_status = ?receipt.provider_status,
                "contact verification challenge delivered"
            );
        }
        Err(error) => {
            if let Err(consume_error) =
                consume_contact_verification_challenge(&pool, challenge_id).await
            {
                tracing::warn!(
                    challenge_id = %challenge_id,
                    error = %consume_error,
                    "failed to consume undelivered contact verification challenge"
                );
            }
            return Err(contact_delivery_error(error));
        }
    }
    tracing::info!(
        challenge_id = %challenge_id,
        contact_id = %command.contact_id,
        contact_point_id = %contact_point.id,
        delivery_channel = delivery.channel.as_str(),
        "contact verification challenge created"
    );
    Ok(ContactVerificationStartResponse {
        challenge_id,
        contact_point,
        delivery_channel: delivery.channel,
        expires_at,
    })
}

/// Resolves the active session channel for a contact.
pub async fn resolve_contact_session_channel(
    pool: &sqlx::PgPool,
    contact: &ContactRef,
    channel_ref: ChannelRef,
) -> Result<ResolvedSessionChannel> {
    match channel_ref {
        ChannelRef::Chat {
            conversation_id,
            user_id,
            client_session_id,
        } => {
            if conversation_id.trim().is_empty() {
                return Err(ContactError::terminal(
                    400,
                    "chat conversation_id is required",
                ));
            }
            let display_name = Some(
                user_id
                    .clone()
                    .unwrap_or_else(|| format!("chat:{conversation_id}")),
            );
            let account = upsert_external_channel_account(
                pool,
                contact,
                Channel::Chat,
                None,
                user_id.as_deref().unwrap_or(conversation_id.as_str()),
                display_name,
            )
            .await?;
            Ok(ResolvedSessionChannel {
                channel_ref: ChannelRef::Chat {
                    conversation_id,
                    user_id,
                    client_session_id,
                },
                channel_account: Some(account),
                contact_point_id: None,
            })
        }
        ChannelRef::Slack {
            team_id,
            slack_channel_id,
            thread_ts,
            user_id,
        } => {
            let user_id = user_id.ok_or_else(|| {
                ContactError::terminal(400, "slack channel route requires user_id")
            })?;
            let account = upsert_external_channel_account(
                pool,
                contact,
                Channel::Slack,
                team_id.as_deref(),
                &user_id,
                Some(format!("<@{user_id}>")),
            )
            .await?;
            Ok(ResolvedSessionChannel {
                channel_ref: ChannelRef::Slack {
                    team_id,
                    slack_channel_id,
                    thread_ts,
                    user_id: Some(user_id),
                },
                channel_account: Some(account),
                contact_point_id: None,
            })
        }
        ChannelRef::Email { channel_account_id } => {
            resolve_contact_point_channel_account(
                pool,
                contact,
                channel_account_id,
                Channel::Email,
                ContactPointKind::Email,
            )
            .await
        }
        ChannelRef::Sms { channel_account_id } => {
            resolve_contact_point_channel_account(
                pool,
                contact,
                channel_account_id,
                Channel::Sms,
                ContactPointKind::Phone,
            )
            .await
        }
    }
}

/// Returns the contact id a session is promoted from, when promotion is allowed.
pub async fn promoted_from_contact(
    pool: &sqlx::PgPool,
    meta: &SessionMeta,
    contact: &ContactRef,
    tenant_id: TenantId,
) -> Result<Option<ContactId>> {
    let Some(current) = meta.contact.as_ref() else {
        return Err(ContactError::terminal(
            403,
            "session has no contact binding",
        ));
    };
    if current.tenant_id != contact.tenant_id || current.tenant_id != tenant_id {
        return Err(ContactError::terminal(
            403,
            "session contact boundary mismatch",
        ));
    }
    if current.contact_id == contact.contact_id {
        return Ok(None);
    }
    if contact_is_merged_into(pool, tenant_id, current.contact_id, contact.contact_id).await? {
        return Ok(Some(current.contact_id));
    }
    Err(ContactError::terminal(
        403,
        "session contact is not linked to verified contact",
    ))
}

/// Completes an OTP verification challenge and returns the canonical verified contact.
pub async fn complete_contact_verification(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    challenge_id: ContactVerificationChallengeId,
    code: String,
) -> Result<ContactRef> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| ContactError::database("begin contact verification completion", error))?;
    let challenge = sqlx::query(
        r#"
        SELECT c.contact_point_id, c.code_hash, c.expires_at, c.consumed_at, c.attempts,
               p.kind, p.normalized_hash, p.display_value
        FROM contact_verification_challenges c
        JOIN contact_points p ON p.id = c.contact_point_id
        WHERE c.id = $1 AND c.contact_id = $2 AND c.tenant_id = $3
        FOR UPDATE
        "#,
    )
    .bind(challenge_id.0)
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| ContactError::database("load contact verification challenge", error))?
    .ok_or_else(|| ContactError::terminal(404, "verification challenge not found"))?;
    let consumed_at = challenge
        .try_get::<Option<DateTime<Utc>>, _>("consumed_at")
        .map_err(|error| ContactError::database("read challenge consumed_at", error))?;
    if consumed_at.is_some() {
        return Err(ContactError::terminal(
            409,
            "verification challenge already used",
        ));
    }
    let expires_at = challenge
        .try_get::<DateTime<Utc>, _>("expires_at")
        .map_err(|error| ContactError::database("read challenge expires_at", error))?;
    if expires_at < Utc::now() {
        return Err(ContactError::terminal(
            410,
            "verification challenge expired",
        ));
    }
    let attempts = challenge
        .try_get::<i32, _>("attempts")
        .map_err(|error| ContactError::database("read challenge attempts", error))?;
    if attempts >= MAX_VERIFICATION_ATTEMPTS {
        return Err(ContactError::terminal(
            429,
            "verification challenge attempts exceeded",
        ));
    }
    let stored_hash = challenge
        .try_get::<String, _>("code_hash")
        .map_err(|error| ContactError::database("read challenge code hash", error))?;
    if stored_hash != hash_verification_code(challenge_id, &code) {
        sqlx::query(
            r#"
            UPDATE contact_verification_challenges
            SET attempts = attempts + 1,
                consumed_at = CASE
                    WHEN attempts + 1 >= $2 THEN NOW()
                    ELSE consumed_at
                END
            WHERE id = $1
            "#,
        )
        .bind(challenge_id.0)
        .bind(MAX_VERIFICATION_ATTEMPTS)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ContactError::database("increment verification attempts", error))?;
        transaction.commit().await.map_err(|error| {
            ContactError::database("commit invalid verification attempt", error)
        })?;
        return Err(ContactError::terminal(403, "invalid verification code"));
    }

    let point_id = ContactPointId(
        challenge
            .try_get::<Uuid, _>("contact_point_id")
            .map_err(|error| ContactError::database("read challenge contact point", error))?,
    );
    let kind = challenge
        .try_get::<String, _>("kind")
        .map_err(|error| ContactError::database("read contact point kind", error))?;
    let point_kind = parse_contact_point_kind(&kind)?;
    let normalized_hash = challenge
        .try_get::<String, _>("normalized_hash")
        .map_err(|error| ContactError::database("read contact point hash", error))?;
    let display_value = challenge
        .try_get::<Option<String>, _>("display_value")
        .map_err(|error| ContactError::database("read contact point display value", error))?;
    let canonical_id = existing_verified_contact(
        &mut transaction,
        tenant_id,
        point_kind.as_str(),
        &normalized_hash,
        contact_id,
    )
    .await?;

    if let Some(canonical_id) = canonical_id {
        sqlx::query(
            r#"
            UPDATE contacts
            SET state = 'merged', canonical_contact_id = $1, merged_at = NOW(), updated_at = NOW()
            WHERE id = $2 AND tenant_id = $3
            "#,
        )
        .bind(canonical_id.0)
        .bind(contact_id.0)
        .bind(tenant_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ContactError::database("merge contact", error))?;
    } else {
        sqlx::query(
            r#"
            UPDATE contact_points
            SET verified = TRUE, verified_at = NOW(), updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(point_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ContactError::database("mark contact point verified", error))?;
        sqlx::query(
            "UPDATE contacts SET state = 'verified', updated_at = NOW() WHERE id = $1 AND tenant_id = $2",
        )
        .bind(contact_id.0)
        .bind(tenant_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ContactError::database("mark contact verified", error))?;
        upsert_verified_contact_point_channel_account(
            &mut transaction,
            tenant_id,
            contact_id,
            point_id,
            point_kind,
            display_value.as_deref(),
        )
        .await?;
    }

    sqlx::query(
        r#"
        UPDATE contact_token_grants
        SET revoked_at = NOW()
        WHERE contact_id = $1
          AND tenant_id = $2
          AND revoked_at IS NULL
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .execute(&mut *transaction)
    .await
    .map_err(|error| {
        ContactError::database("revoke pre-verification contact token grants", error)
    })?;

    sqlx::query("UPDATE contact_verification_challenges SET consumed_at = NOW() WHERE id = $1")
        .bind(challenge_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ContactError::database("consume contact verification challenge", error))?;

    transaction
        .commit()
        .await
        .map_err(|error| ContactError::database("commit contact verification completion", error))?;

    load_contact_ref(pool, tenant_id, canonical_id.unwrap_or(contact_id)).await
}

async fn existing_verified_contact(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: TenantId,
    kind: &str,
    normalized_hash: &str,
    excluded_contact_id: ContactId,
) -> Result<Option<ContactId>> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT contact_id
        FROM contact_points
        WHERE tenant_id = $1
          AND kind = $2
          AND normalized_hash = $3
          AND verified = TRUE
          AND contact_id <> $4
        LIMIT 1
        "#,
    )
    .bind(tenant_id.0)
    .bind(kind)
    .bind(normalized_hash)
    .bind(excluded_contact_id.0)
    .fetch_optional(&mut **tx)
    .await
    .map(|value| value.map(ContactId))
    .map_err(|error| ContactError::database("find existing verified contact", error))
}

async fn upsert_verified_contact_point_channel_account(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: TenantId,
    contact_id: ContactId,
    point_id: ContactPointId,
    kind: ContactPointKind,
    display_name: Option<&str>,
) -> Result<Option<ChannelAccountRef>> {
    let channel = match kind {
        ContactPointKind::Email => Channel::Email,
        ContactPointKind::Phone => Channel::Sms,
        ContactPointKind::ExternalId | ContactPointKind::AnonymousHandle => return Ok(None),
    };
    let updated = sqlx::query(
        r#"
        UPDATE contact_channel_accounts
        SET assurance = 'otp_verified',
            display_name = COALESCE($1, display_name),
            last_seen_at = NOW()
        WHERE tenant_id = $2
          AND contact_point_id = $3
          AND channel = $4
          AND merged_into_id IS NULL
        RETURNING id, display_name
        "#,
    )
    .bind(display_name)
    .bind(tenant_id.0)
    .bind(point_id.0)
    .bind(channel.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| ContactError::database("update verified channel account", error))?;

    if let Some(row) = updated {
        return Ok(Some(ChannelAccountRef {
            channel_account_id: ChannelAccountId(row.try_get::<Uuid, _>("id").map_err(
                |error| ContactError::database("read verified channel account id", error),
            )?),
            contact_point_id: Some(point_id),
            channel,
            display_name: row
                .try_get::<Option<String>, _>("display_name")
                .map_err(|error| {
                    ContactError::database("read verified channel account display", error)
                })?,
        }));
    }

    let account_id = ChannelAccountId::new();
    sqlx::query(
        r#"
        INSERT INTO contact_channel_accounts
            (id, tenant_id, storage_partition_id, contact_id, contact_point_id, channel,
             external_user_key, display_name, assurance, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'otp_verified', $9)
        "#,
    )
    .bind(account_id.0)
    .bind(tenant_id.0)
    .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
    .bind(contact_id.0)
    .bind(point_id.0)
    .bind(channel.as_str())
    .bind(point_id.to_string())
    .bind(display_name)
    .bind(serde_json::json!({ "source": "contact_verification" }))
    .execute(&mut **tx)
    .await
    .map_err(|error| ContactError::database("insert verified channel account", error))?;

    Ok(Some(ChannelAccountRef {
        channel_account_id: account_id,
        contact_point_id: Some(point_id),
        channel,
        display_name: display_name.map(ToOwned::to_owned),
    }))
}

async fn consume_contact_verification_challenge(
    pool: &sqlx::PgPool,
    challenge_id: ContactVerificationChallengeId,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE contact_verification_challenges
        SET consumed_at = NOW()
        WHERE id = $1 AND consumed_at IS NULL
        "#,
    )
    .bind(challenge_id.0)
    .execute(pool)
    .await?;
    Ok(())
}

async fn upsert_external_channel_account(
    pool: &sqlx::PgPool,
    contact: &ContactRef,
    channel: Channel,
    external_tenant_key: Option<&str>,
    external_user_key: &str,
    display_name: Option<String>,
) -> Result<ChannelAccountRef> {
    if external_user_key.trim().is_empty() {
        return Err(ContactError::terminal(400, "channel user id is required"));
    }
    let row = sqlx::query(
        r#"
        SELECT id, contact_id, display_name
        FROM contact_channel_accounts
        WHERE tenant_id = $1
          AND channel = $2
          AND COALESCE(external_tenant_key, '') = COALESCE($3, '')
          AND external_user_key = $4
          AND merged_into_id IS NULL
        "#,
    )
    .bind(contact.tenant_id.0)
    .bind(channel.as_str())
    .bind(external_tenant_key)
    .bind(external_user_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| ContactError::database("load channel account", error))?;

    if let Some(row) = row {
        let account_contact_id = ContactId(
            row.try_get::<Uuid, _>("contact_id")
                .map_err(|error| ContactError::database("read channel account contact", error))?,
        );
        if !contact_allows_channel_contact(
            contact.contact_id,
            contact.canonical_contact_id,
            account_contact_id,
        ) {
            return Err(ContactError::terminal(
                403,
                "channel account belongs to another contact",
            ));
        }
        let account_id = ChannelAccountId(
            row.try_get::<Uuid, _>("id")
                .map_err(|error| ContactError::database("read channel account id", error))?,
        );
        sqlx::query(
            r#"
            UPDATE contact_channel_accounts
            SET last_seen_at = NOW(), display_name = COALESCE($1, display_name)
            WHERE id = $2
            "#,
        )
        .bind(display_name.as_deref())
        .bind(account_id.0)
        .execute(pool)
        .await
        .map_err(|error| ContactError::database("touch channel account", error))?;
        return Ok(ChannelAccountRef {
            channel_account_id: account_id,
            contact_point_id: None,
            channel,
            display_name: display_name.or_else(|| {
                row.try_get::<Option<String>, _>("display_name")
                    .ok()
                    .flatten()
            }),
        });
    }

    let account_id = ChannelAccountId::new();
    sqlx::query(
        r#"
        INSERT INTO contact_channel_accounts
            (id, tenant_id, storage_partition_id, contact_id, channel, external_tenant_key,
             external_user_key, display_name, assurance, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'provider_asserted', $9)
        "#,
    )
    .bind(account_id.0)
    .bind(contact.tenant_id.0)
    .bind(StoragePartitionId::for_tenant(contact.tenant_id).as_str())
    .bind(contact.contact_id.0)
    .bind(channel.as_str())
    .bind(external_tenant_key)
    .bind(external_user_key)
    .bind(display_name.as_deref())
    .bind(serde_json::json!({ "source": "session_channel" }))
    .execute(pool)
    .await
    .map_err(|error| ContactError::database("insert channel account", error))?;
    Ok(ChannelAccountRef {
        channel_account_id: account_id,
        contact_point_id: None,
        channel,
        display_name,
    })
}

async fn resolve_contact_point_channel_account(
    pool: &sqlx::PgPool,
    contact: &ContactRef,
    channel_account_id: ChannelAccountId,
    channel: Channel,
    expected_kind: ContactPointKind,
) -> Result<ResolvedSessionChannel> {
    let row = sqlx::query(
        r#"
        SELECT a.id, a.contact_id, a.contact_point_id, a.display_name,
               p.kind, p.verified
        FROM contact_channel_accounts a
        JOIN contact_points p ON p.id = a.contact_point_id
        WHERE a.id = $1
          AND a.tenant_id = $2
          AND a.channel = $3
          AND a.merged_into_id IS NULL
        "#,
    )
    .bind(channel_account_id.0)
    .bind(contact.tenant_id.0)
    .bind(channel.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|error| ContactError::database("load contact channel account", error))?
    .ok_or_else(|| ContactError::terminal(404, "channel account not found"))?;

    let account_contact_id = ContactId(
        row.try_get::<Uuid, _>("contact_id")
            .map_err(|error| ContactError::database("read channel account contact", error))?,
    );
    if !contact_allows_channel_contact(
        contact.contact_id,
        contact.canonical_contact_id,
        account_contact_id,
    ) {
        return Err(ContactError::terminal(
            403,
            "channel account belongs to another contact",
        ));
    }
    let point_id = ContactPointId(
        row.try_get::<Uuid, _>("contact_point_id")
            .map_err(|error| ContactError::database("read channel account contact point", error))?,
    );
    let kind = row.try_get::<String, _>("kind").map_err(|error| {
        ContactError::database("read channel account contact point kind", error)
    })?;
    if parse_contact_point_kind(&kind)? != expected_kind {
        return Err(ContactError::terminal(
            400,
            "channel account contact point kind mismatch",
        ));
    }
    let verified = row
        .try_get::<bool, _>("verified")
        .map_err(|error| ContactError::database("read channel account verification", error))?;
    if !verified {
        return Err(ContactError::terminal(
            403,
            "channel account contact point is not verified",
        ));
    }
    let channel_ref = match channel {
        Channel::Email => ChannelRef::Email { channel_account_id },
        Channel::Sms => ChannelRef::Sms { channel_account_id },
        Channel::Chat | Channel::Slack => {
            return Err(ContactError::terminal(
                400,
                "unsupported contact point channel",
            ));
        }
    };
    Ok(ResolvedSessionChannel {
        channel_ref,
        channel_account: Some(ChannelAccountRef {
            channel_account_id,
            contact_point_id: Some(point_id),
            channel,
            display_name: row
                .try_get::<Option<String>, _>("display_name")
                .map_err(|error| {
                    ContactError::database("read channel account display name", error)
                })?,
        }),
        contact_point_id: Some(point_id),
    })
}

async fn ensure_contact_in_tenant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> Result<()> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM contacts WHERE id = $1 AND tenant_id = $2)",
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| ContactError::database("check contact workspace", error))?;
    if exists {
        Ok(())
    } else {
        Err(ContactError::terminal(404, "contact not found"))
    }
}

async fn contact_is_merged_into(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    canonical_contact_id: ContactId,
) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM contacts
            WHERE id = $1
              AND tenant_id = $2
              AND canonical_contact_id = $3
              AND state = 'merged'
        )
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(canonical_contact_id.0)
    .fetch_one(pool)
    .await
    .map_err(|error| ContactError::database("check promoted contact linkage", error))
}

fn contact_delivery_error(error: MoaError) -> ContactError {
    let error_kind = match &error {
        MoaError::ConfigError(_) | MoaError::MissingEnvironmentVariable(_) => "configuration",
        MoaError::ValidationError(_) => "validation",
        MoaError::RateLimited { .. } => "rate_limited",
        MoaError::HttpStatus { status, .. } if (500..600).contains(status) => "provider_5xx",
        MoaError::HttpStatus { .. } => "provider_http",
        MoaError::ProviderQuirk(_) => "provider_retryable",
        MoaError::ProviderError(_) => "provider",
        _ => "other",
    };
    tracing::warn!(
        error_kind,
        "contact delivery failed before verification challenge could be used"
    );
    match error {
        MoaError::ConfigError(_) | MoaError::MissingEnvironmentVariable(_) => {
            ContactError::terminal(503, "contact delivery provider is not configured")
        }
        MoaError::ValidationError(_) => {
            ContactError::terminal(400, "contact delivery request is invalid")
        }
        MoaError::RateLimited { .. } => {
            ContactError::terminal(429, "contact delivery provider is rate limited")
        }
        MoaError::HttpStatus { status, .. } if (500..600).contains(&status) => {
            ContactError::terminal(502, "contact delivery provider failed")
        }
        _ => ContactError::terminal(502, "contact delivery provider failed"),
    }
}

async fn insert_contact_point(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    contact_point_hash_key_hex: &str,
    tenant_id: TenantId,
    contact_id: ContactId,
    point: ContactPointInput,
    verified: bool,
) -> Result<ContactPointRef> {
    let normalized = normalize_contact_point(point.kind, &point.value)?;
    let normalized_hash = hash_contact_point_with_key_hex(
        tenant_id,
        point.kind,
        &normalized,
        contact_point_hash_key_hex,
    )?;
    let point_id = ContactPointId::new();
    let verified_at = verified.then(Utc::now);
    let row = sqlx::query(
        r#"
        INSERT INTO contact_points
            (id, contact_id, tenant_id, storage_partition_id, kind, normalized_hash, display_value, verified, verified_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (tenant_id, storage_partition_id, contact_id, kind, normalized_hash)
        DO UPDATE SET
            display_value = COALESCE(EXCLUDED.display_value, contact_points.display_value),
            verified = contact_points.verified OR EXCLUDED.verified,
            verified_at = COALESCE(contact_points.verified_at, EXCLUDED.verified_at),
            updated_at = NOW()
        RETURNING id, display_value, verified, verified_at
        "#,
    )
    .bind(point_id.0)
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
    .bind(point.kind.as_str())
    .bind(&normalized_hash)
    .bind(point.display_value.as_deref())
    .bind(verified)
    .bind(verified_at)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| ContactError::database("upsert contact point", error))?;
    Ok(ContactPointRef {
        id: ContactPointId(
            row.try_get::<Uuid, _>("id")
                .map_err(|error| ContactError::database("read contact point id", error))?,
        ),
        kind: point.kind,
        display_value: row
            .try_get::<Option<String>, _>("display_value")
            .map_err(|error| ContactError::database("read contact point display value", error))?,
        verified: row
            .try_get::<bool, _>("verified")
            .map_err(|error| ContactError::database("read contact point verified flag", error))?,
        verified_at: row
            .try_get::<Option<DateTime<Utc>>, _>("verified_at")
            .map_err(|error| ContactError::database("read contact point verified_at", error))?,
    })
}
