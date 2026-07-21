//! Application service for persisted contact verification and OTP delivery.

use std::future::Future;

use chrono::{DateTime, Utc};
use moa_core::{
    error::MoaError, types::channel::Channel, types::contact::ContactId,
    types::contact::ContactPointInput, types::contact::ContactVerificationChallengeId,
    types::contact::ContactVerificationStartResponse, types::identifiers::TenantId,
};
use moa_messaging::{DeliveryMessage, DeliveryReceipt, ProviderDeliverySink};

use crate::domain::contact_point_delivery;
use crate::repository::{
    CreatedContactVerificationChallenge, consume_contact_verification_challenge,
    create_contact_verification_challenge,
};
use crate::{Error, Result};

/// Narrow outbound payload for one contact verification OTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactOtp {
    /// Persisted challenge associated with this delivery attempt.
    pub challenge_id: ContactVerificationChallengeId,
    /// Tenant that owns the contact and destination.
    pub tenant_id: TenantId,
    /// Contact receiving the verification code.
    pub contact_id: ContactId,
    /// Email or SMS delivery channel.
    pub channel: Channel,
    /// Normalized email address or phone number.
    pub destination: String,
    /// One-time verification code.
    pub code: String,
    /// Challenge expiration rendered into the provider message.
    pub expires_at: DateTime<Utc>,
}

/// Outbound delivery port limited to contact verification OTPs.
pub trait ContactOtpDelivery: Send + Sync {
    /// Delivers one previously persisted contact verification OTP.
    fn deliver_contact_verification_otp(
        &self,
        otp: ContactOtp,
    ) -> impl Future<Output = moa_core::error::Result<DeliveryReceipt>> + Send;
}

impl ContactOtpDelivery for ProviderDeliverySink {
    async fn deliver_contact_verification_otp(
        &self,
        otp: ContactOtp,
    ) -> moa_core::error::Result<DeliveryReceipt> {
        self.deliver(DeliveryMessage::contact_verification_otp(
            otp.tenant_id.0,
            otp.contact_id,
            otp.channel,
            otp.destination,
            otp.code,
            otp.expires_at,
        ))
        .await
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
}

/// Coordinates persisted challenges with one injected OTP delivery implementation.
pub struct ContactVerifier<D> {
    pool: sqlx::PgPool,
    delivery: D,
}

impl<D> ContactVerifier<D>
where
    D: ContactOtpDelivery,
{
    /// Creates a contact verification service from its persistence and delivery dependencies.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, delivery: D) -> Self {
        Self { pool, delivery }
    }

    /// Persists a challenge, delivers its OTP, and consumes it if delivery fails.
    pub async fn start_verification(
        &self,
        command: ContactVerificationStartCommand,
    ) -> Result<ContactVerificationStartResponse> {
        let delivery = contact_point_delivery(
            command.contact_point.kind,
            &command.contact_point.value,
            command.requested_channel,
        )?;
        let CreatedContactVerificationChallenge {
            challenge_id,
            code,
            expires_at,
            contact_point,
        } = create_contact_verification_challenge(
            &self.pool,
            &command.contact_point_hash_key_hex,
            command.tenant_id,
            command.contact_id,
            command.contact_point,
            command.ttl_seconds,
        )
        .await?;
        let otp = ContactOtp {
            challenge_id,
            tenant_id: command.tenant_id,
            contact_id: command.contact_id,
            channel: delivery.channel,
            destination: delivery.destination,
            code,
            expires_at,
        };
        match self.delivery.deliver_contact_verification_otp(otp).await {
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
                consume_undelivered_challenge(&self.pool, challenge_id).await;
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
}

/// Maps an outbound provider failure to the stable contact-service error contract.
pub fn contact_delivery_error(error: MoaError) -> Error {
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
            Error::terminal(503, "contact delivery provider is not configured")
        }
        MoaError::ValidationError(_) => Error::terminal(400, "contact delivery request is invalid"),
        MoaError::RateLimited { .. } => {
            Error::terminal(429, "contact delivery provider is rate limited")
        }
        MoaError::HttpStatus { status, .. } if (500..600).contains(&status) => {
            Error::terminal(502, "contact delivery provider failed")
        }
        _ => Error::terminal(502, "contact delivery provider failed"),
    }
}

async fn consume_undelivered_challenge(
    pool: &sqlx::PgPool,
    challenge_id: ContactVerificationChallengeId,
) {
    if let Err(error) = consume_contact_verification_challenge(pool, challenge_id).await {
        tracing::warn!(
            challenge_id = %challenge_id,
            error = %error,
            "failed to consume undelivered contact verification challenge"
        );
    }
}
