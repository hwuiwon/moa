//! Contact channel-account persistence and route resolution.

use moa_core::{
    types::channel::Channel, types::channel::ChannelAccountId, types::channel::ChannelAccountRef,
    types::channel::ChannelRef, types::contact::ContactId, types::contact::ContactPointId,
    types::contact::ContactPointKind, types::contact::ContactRef,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use sqlx::Row as _;
use uuid::Uuid;

use crate::domain::{contact_allows_channel_contact, parse_contact_point_kind};
use crate::{Error, Result};

use super::row_mapping::RowExt as _;

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
                return Err(Error::terminal(400, "chat conversation_id is required"));
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
            let user_id = user_id
                .ok_or_else(|| Error::terminal(400, "slack channel route requires user_id"))?;
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

pub(super) async fn upsert_verified_contact_point_channel_account(
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
    .map_err(|error| Error::database("update verified channel account", error))?;

    if let Some(row) = updated {
        return Ok(Some(ChannelAccountRef {
            channel_account_id: ChannelAccountId(row.col::<Uuid>("id")?),
            contact_point_id: Some(point_id),
            channel,
            display_name: row.col::<Option<String>>("display_name")?,
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
    .map_err(|error| Error::database("insert verified channel account", error))?;

    Ok(Some(ChannelAccountRef {
        channel_account_id: account_id,
        contact_point_id: Some(point_id),
        channel,
        display_name: display_name.map(ToOwned::to_owned),
    }))
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
        return Err(Error::terminal(400, "channel user id is required"));
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
    .map_err(|error| Error::database("load channel account", error))?;

    if let Some(row) = row {
        let account_contact_id = ContactId(row.col::<Uuid>("contact_id")?);
        if !contact_allows_channel_contact(
            contact.contact_id,
            contact.canonical_contact_id,
            account_contact_id,
        ) {
            return Err(Error::terminal(
                403,
                "channel account belongs to another contact",
            ));
        }
        let account_id = ChannelAccountId(row.col::<Uuid>("id")?);
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
        .map_err(|error| Error::database("touch channel account", error))?;
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
    .map_err(|error| Error::database("insert channel account", error))?;
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
    .map_err(|error| Error::database("load contact channel account", error))?
    .ok_or_else(|| Error::terminal(404, "channel account not found"))?;

    let account_contact_id = ContactId(row.col::<Uuid>("contact_id")?);
    if !contact_allows_channel_contact(
        contact.contact_id,
        contact.canonical_contact_id,
        account_contact_id,
    ) {
        return Err(Error::terminal(
            403,
            "channel account belongs to another contact",
        ));
    }
    let point_id = ContactPointId(row.col::<Uuid>("contact_point_id")?);
    let kind = row.col::<String>("kind")?;
    if parse_contact_point_kind(&kind)? != expected_kind {
        return Err(Error::terminal(
            400,
            "channel account contact point kind mismatch",
        ));
    }
    let verified = row.col::<bool>("verified")?;
    if !verified {
        return Err(Error::terminal(
            403,
            "channel account contact point is not verified",
        ));
    }
    let channel_ref = match channel {
        Channel::Email => ChannelRef::Email { channel_account_id },
        Channel::Sms => ChannelRef::Sms { channel_account_id },
        Channel::Chat | Channel::Slack => {
            return Err(Error::terminal(400, "unsupported contact point channel"));
        }
    };
    Ok(ResolvedSessionChannel {
        channel_ref,
        channel_account: Some(ChannelAccountRef {
            channel_account_id,
            contact_point_id: Some(point_id),
            channel,
            display_name: row.col::<Option<String>>("display_name")?,
        }),
        contact_point_id: Some(point_id),
    })
}
