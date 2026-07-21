//! Contact verification challenge persistence and completion.

use chrono::{DateTime, Duration, Utc};
use moa_core::{
    types::contact::ContactId, types::contact::ContactPointId, types::contact::ContactPointInput,
    types::contact::ContactPointRef, types::contact::ContactRef,
    types::contact::ContactVerificationChallengeId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
};
use uuid::Uuid;

use crate::domain::{hash_verification_code, parse_contact_point_kind};
use crate::{Error, Result};

use super::channel_accounts::upsert_verified_contact_point_channel_account;
use super::contacts::{ensure_contact_in_tenant, insert_contact_point, load_contact_ref};
use super::row_mapping::RowExt as _;

const MAX_VERIFICATION_ATTEMPTS: i32 = 5;

pub(crate) struct CreatedContactVerificationChallenge {
    pub(crate) challenge_id: ContactVerificationChallengeId,
    pub(crate) code: String,
    pub(crate) expires_at: DateTime<Utc>,
    pub(crate) contact_point: ContactPointRef,
}

pub(crate) async fn create_contact_verification_challenge(
    pool: &sqlx::PgPool,
    contact_point_hash_key_hex: &str,
    tenant_id: TenantId,
    contact_id: ContactId,
    contact_point: ContactPointInput,
    ttl_seconds: i64,
) -> Result<CreatedContactVerificationChallenge> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| Error::database("begin contact verification", error))?;
    ensure_contact_in_tenant(&mut transaction, tenant_id, contact_id).await?;
    let contact_point = insert_contact_point(
        &mut transaction,
        contact_point_hash_key_hex,
        tenant_id,
        contact_id,
        contact_point,
        false,
    )
    .await?;
    let challenge_id = ContactVerificationChallengeId::new();
    let code = crate::domain::verification_code();
    let expires_at = Utc::now() + Duration::seconds(ttl_seconds);
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
    .bind(contact_id.0)
    .bind(contact_point.id.0)
    .bind(tenant_id.0)
    .execute(&mut *transaction)
    .await
    .map_err(|error| Error::database("close previous contact verification challenges", error))?;
    sqlx::query(
        r#"
        INSERT INTO contact_verification_challenges
            (id, contact_id, contact_point_id, tenant_id, storage_partition_id, code_hash, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(challenge_id.0)
    .bind(contact_id.0)
    .bind(contact_point.id.0)
    .bind(tenant_id.0)
    .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
    .bind(hash_verification_code(challenge_id, &code))
    .bind(expires_at)
    .execute(&mut *transaction)
    .await
    .map_err(|error| Error::database("insert contact verification challenge", error))?;
    transaction
        .commit()
        .await
        .map_err(|error| Error::database("commit contact verification", error))?;
    Ok(CreatedContactVerificationChallenge {
        challenge_id,
        code,
        expires_at,
        contact_point,
    })
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
        .map_err(|error| Error::database("begin contact verification completion", error))?;
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
    .map_err(|error| Error::database("load contact verification challenge", error))?
    .ok_or_else(|| Error::terminal(404, "verification challenge not found"))?;
    let consumed_at = challenge.col::<Option<DateTime<Utc>>>("consumed_at")?;
    if consumed_at.is_some() {
        return Err(Error::terminal(409, "verification challenge already used"));
    }
    let expires_at = challenge.col::<DateTime<Utc>>("expires_at")?;
    if expires_at < Utc::now() {
        return Err(Error::terminal(410, "verification challenge expired"));
    }
    let attempts = challenge.col::<i32>("attempts")?;
    if attempts >= MAX_VERIFICATION_ATTEMPTS {
        return Err(Error::terminal(
            429,
            "verification challenge attempts exceeded",
        ));
    }
    let stored_hash = challenge.col::<String>("code_hash")?;
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
        .map_err(|error| Error::database("increment verification attempts", error))?;
        transaction
            .commit()
            .await
            .map_err(|error| Error::database("commit invalid verification attempt", error))?;
        return Err(Error::terminal(403, "invalid verification code"));
    }

    let point_id = ContactPointId(challenge.col::<Uuid>("contact_point_id")?);
    let kind = challenge.col::<String>("kind")?;
    let point_kind = parse_contact_point_kind(&kind)?;
    let normalized_hash = challenge.col::<String>("normalized_hash")?;
    let display_value = challenge.col::<Option<String>>("display_value")?;
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
        .map_err(|error| Error::database("merge contact", error))?;
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
        .map_err(|error| Error::database("mark contact point verified", error))?;
        sqlx::query(
            "UPDATE contacts SET state = 'verified', updated_at = NOW() WHERE id = $1 AND tenant_id = $2",
        )
        .bind(contact_id.0)
        .bind(tenant_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| Error::database("mark contact verified", error))?;
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
    .map_err(|error| Error::database("revoke pre-verification contact token grants", error))?;

    sqlx::query("UPDATE contact_verification_challenges SET consumed_at = NOW() WHERE id = $1")
        .bind(challenge_id.0)
        .execute(&mut *transaction)
        .await
        .map_err(|error| Error::database("consume contact verification challenge", error))?;

    transaction
        .commit()
        .await
        .map_err(|error| Error::database("commit contact verification completion", error))?;

    load_contact_ref(pool, tenant_id, canonical_id.unwrap_or(contact_id)).await
}

pub(crate) async fn consume_contact_verification_challenge(
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
    .map_err(|error| Error::database("find existing verified contact", error))
}
