//! Channel-neutral delivery helpers for contact-facing messages.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    Channel, ContactId, Credential, CredentialVault, MessagingConfig, MoaError, Result,
    StoredCredentialMetadata,
};
use tracing::Instrument;
use uuid::Uuid;

#[cfg(feature = "postmark")]
use crate::postmark::{
    POSTMARK_SERVER_API_TOKEN_ENV, POSTMARK_SERVER_TOKEN_SERVICE, PostmarkEmailClient,
    PostmarkEmailMessage,
};
#[cfg(feature = "twilio")]
use crate::twilio::{
    TWILIO_ACCOUNT_SID_ENV, TWILIO_ACCOUNT_SID_SERVICE, TWILIO_API_KEY_SECRET_ENV,
    TWILIO_API_KEY_SECRET_SERVICE, TWILIO_API_KEY_SID_ENV, TWILIO_API_KEY_SID_SERVICE,
    TWILIO_AUTH_TOKEN_ENV, TWILIO_AUTH_TOKEN_SERVICE, TWILIO_FROM_NUMBER_ENV,
    TWILIO_FROM_NUMBER_SERVICE, TWILIO_MESSAGING_SERVICE_SID_ENV,
    TWILIO_MESSAGING_SERVICE_SID_SERVICE, TwilioSmsClient, TwilioSmsMessage,
};

/// Delivery use case used for routing, metadata, and provider tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryPurpose {
    /// One-time contact-point verification code.
    ContactVerification,
}

impl DeliveryPurpose {
    /// Returns the stable telemetry and provider-tag representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContactVerification => "contact_verification",
        }
    }
}

/// Outbound message after a caller has selected a delivery channel and recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryMessage {
    /// Tenant boundary that owns the recipient contact.
    pub tenant_id: Uuid,
    /// Contact receiving the message.
    pub contact_id: ContactId,
    /// Delivery use case.
    pub purpose: DeliveryPurpose,
    /// Selected delivery channel.
    pub channel: Channel,
    /// Normalized destination address or phone number.
    pub to: String,
    /// Email subject. Ignored for SMS.
    pub subject: Option<String>,
    /// Plain-text body sent to the provider.
    pub text_body: String,
    /// Optional HTML body for email.
    pub html_body: Option<String>,
    /// Provider-safe metadata.
    pub metadata: BTreeMap<String, String>,
}

impl DeliveryMessage {
    /// Builds an OTP delivery message for contact-point verification.
    #[must_use]
    pub fn contact_verification_otp(
        tenant_id: Uuid,
        contact_id: ContactId,
        channel: Channel,
        to: impl Into<String>,
        code: impl AsRef<str>,
        expires_at: DateTime<Utc>,
    ) -> Self {
        let code = code.as_ref();
        let text_body = format!(
            "Your MOA verification code is {code}. It expires at {}.",
            expires_at.to_rfc3339()
        );
        let html_body = format!(
            "<p>Your MOA verification code is <strong>{code}</strong>.</p><p>It expires at {}.</p>",
            expires_at.to_rfc3339()
        );
        let mut metadata = BTreeMap::new();
        metadata.insert("purpose".to_string(), "contact_verification".to_string());
        metadata.insert("contact_id".to_string(), contact_id.to_string());
        Self {
            tenant_id,
            contact_id,
            purpose: DeliveryPurpose::ContactVerification,
            channel,
            to: to.into(),
            subject: Some("Your MOA verification code".to_string()),
            text_body,
            html_body: Some(html_body),
            metadata,
        }
    }
}

/// Provider delivery acceptance receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReceipt {
    /// Channel used for the send.
    pub channel: Channel,
    /// Provider that accepted the message.
    pub provider: String,
    /// Provider message identifier when available.
    pub provider_message_id: Option<String>,
    /// Provider status when available.
    pub provider_status: Option<String>,
}

/// Delivery sink backed by Postmark email and Twilio SMS clients.
#[derive(Clone)]
pub struct ProviderDeliverySink {
    #[cfg(feature = "postmark")]
    email: Option<PostmarkEmailClient>,
    #[cfg(feature = "twilio")]
    sms: Option<TwilioSmsClient>,
    #[cfg(feature = "postmark")]
    email_from: String,
    #[cfg(feature = "postmark")]
    email_reply_to: Option<String>,
}

impl ProviderDeliverySink {
    /// Creates an empty provider-backed delivery sink.
    #[must_use]
    pub fn empty(email_from: impl Into<String>) -> Self {
        #[cfg(not(feature = "postmark"))]
        let _ = email_from;
        Self {
            #[cfg(feature = "postmark")]
            email: None,
            #[cfg(feature = "twilio")]
            sms: None,
            #[cfg(feature = "postmark")]
            email_from: email_from.into(),
            #[cfg(feature = "postmark")]
            email_reply_to: None,
        }
    }

    /// Builds a provider-backed delivery sink from a credential vault.
    pub async fn from_vault(
        vault: Arc<dyn CredentialVault>,
        scope: &str,
        config: &MessagingConfig,
    ) -> Result<Self> {
        #[cfg(any(feature = "postmark", feature = "twilio"))]
        let mut sink = Self::empty(config.email_from.clone());
        #[cfg(all(any(feature = "postmark", feature = "twilio"), feature = "postmark"))]
        {
            sink = sink.with_email_reply_to(config.email_reply_to.clone());
        }
        #[cfg(not(any(feature = "postmark", feature = "twilio")))]
        let sink = {
            let _ = (vault, scope);
            let _ = &config.email_reply_to;
            Self::empty(config.email_from.clone())
        };
        #[cfg(feature = "postmark")]
        {
            sink.email = optional_postmark_client(vault.clone(), scope, config).await?;
        }
        #[cfg(feature = "twilio")]
        {
            sink.sms = optional_twilio_client(vault, scope, config).await?;
        }
        Ok(sink)
    }

    /// Builds a provider-backed delivery sink from process environment variables.
    pub async fn from_env(scope: &str, config: &MessagingConfig) -> Result<Self> {
        let vault: Arc<dyn CredentialVault> = Arc::new(EnvironmentDeliveryCredentialVault);
        Self::from_vault(vault, scope, config).await
    }

    /// Sets the optional reply-to address for email delivery.
    #[cfg(feature = "postmark")]
    #[must_use]
    pub fn with_email_reply_to(mut self, reply_to: Option<String>) -> Self {
        self.email_reply_to = reply_to;
        self
    }

    /// Adds a Postmark email client.
    #[cfg(feature = "postmark")]
    #[must_use]
    pub fn with_email_client(mut self, client: PostmarkEmailClient) -> Self {
        self.email = Some(client);
        self
    }

    /// Adds a Twilio SMS client.
    #[cfg(feature = "twilio")]
    #[must_use]
    pub fn with_sms_client(mut self, client: TwilioSmsClient) -> Self {
        self.sms = Some(client);
        self
    }

    #[cfg(feature = "postmark")]
    async fn deliver_email(&self, message: DeliveryMessage) -> Result<DeliveryReceipt> {
        let client = self
            .email
            .as_ref()
            .ok_or_else(|| MoaError::ConfigError("email delivery is not configured".to_string()))?;
        let subject = message.subject.as_deref().ok_or_else(|| {
            MoaError::ValidationError("email delivery requires a subject".to_string())
        })?;
        if self.email_from.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "MOA_MESSAGING_EMAIL_FROM is required for email delivery".to_string(),
            ));
        }
        let mut email =
            PostmarkEmailMessage::new(self.email_from.clone(), message.to.clone(), subject)
                .with_text_body(message.text_body)
                .with_tag(message.purpose.as_str());
        if let Some(html_body) = message.html_body {
            email = email.with_html_body(html_body);
        }
        if let Some(reply_to) = self.email_reply_to.as_deref() {
            email = email.with_reply_to(reply_to);
        }
        for (key, value) in message.metadata {
            email = email.with_metadata(key, value);
        }
        let result = client.send_email(&email).await?;
        Ok(DeliveryReceipt {
            channel: Channel::Email,
            provider: "postmark".to_string(),
            provider_message_id: Some(result.message_id),
            provider_status: Some(result.message),
        })
    }

    #[cfg(feature = "twilio")]
    async fn deliver_sms(&self, message: DeliveryMessage) -> Result<DeliveryReceipt> {
        let client = self
            .sms
            .as_ref()
            .ok_or_else(|| MoaError::ConfigError("sms delivery is not configured".to_string()))?;
        let sms = TwilioSmsMessage::new(message.to, message.text_body);
        let result = client.send_sms(&sms).await?;
        Ok(DeliveryReceipt {
            channel: Channel::Sms,
            provider: "twilio".to_string(),
            provider_message_id: Some(result.sid),
            provider_status: Some(result.status),
        })
    }
    /// Delivers one already-rendered message through the selected channel.
    pub async fn deliver(&self, message: DeliveryMessage) -> Result<DeliveryReceipt> {
        let span = delivery_span(&message);
        async move {
            match message.channel {
                Channel::Email => {
                    #[cfg(feature = "postmark")]
                    {
                        self.deliver_email(message).await
                    }
                    #[cfg(not(feature = "postmark"))]
                    {
                        let _ = message;
                        Err(MoaError::ConfigError(
                            "email delivery support is not enabled".to_string(),
                        ))
                    }
                }
                Channel::Sms => {
                    #[cfg(feature = "twilio")]
                    {
                        self.deliver_sms(message).await
                    }
                    #[cfg(not(feature = "twilio"))]
                    {
                        let _ = message;
                        Err(MoaError::ConfigError(
                            "sms delivery support is not enabled".to_string(),
                        ))
                    }
                }
                Channel::Chat | Channel::Slack => Err(MoaError::ValidationError(format!(
                    "{} is not supported for contact delivery",
                    message.channel
                ))),
            }
        }
        .instrument(span)
        .await
    }
}

/// Environment-backed credential vault for messaging providers.
#[derive(Debug, Default)]
pub struct EnvironmentDeliveryCredentialVault;

#[async_trait]
impl CredentialVault for EnvironmentDeliveryCredentialVault {
    async fn get(&self, service: &str, _scope: &str) -> Result<Credential> {
        #[cfg(any(feature = "postmark", feature = "twilio"))]
        {
            let value = match service {
                #[cfg(feature = "postmark")]
                POSTMARK_SERVER_TOKEN_SERVICE => required_env(POSTMARK_SERVER_API_TOKEN_ENV)?,
                #[cfg(feature = "twilio")]
                TWILIO_ACCOUNT_SID_SERVICE => required_env(TWILIO_ACCOUNT_SID_ENV)?,
                #[cfg(feature = "twilio")]
                TWILIO_AUTH_TOKEN_SERVICE => required_env(TWILIO_AUTH_TOKEN_ENV)?,
                #[cfg(feature = "twilio")]
                TWILIO_API_KEY_SID_SERVICE => required_env(TWILIO_API_KEY_SID_ENV)?,
                #[cfg(feature = "twilio")]
                TWILIO_API_KEY_SECRET_SERVICE => required_env(TWILIO_API_KEY_SECRET_ENV)?,
                #[cfg(feature = "twilio")]
                TWILIO_FROM_NUMBER_SERVICE => required_env(TWILIO_FROM_NUMBER_ENV)?,
                #[cfg(feature = "twilio")]
                TWILIO_MESSAGING_SERVICE_SID_SERVICE => {
                    required_env(TWILIO_MESSAGING_SERVICE_SID_ENV)?
                }
                _ => {
                    return Err(MoaError::MissingEnvironmentVariable(service.to_string()));
                }
            };
            Ok(Credential::Bearer(value))
        }
        #[cfg(not(any(feature = "postmark", feature = "twilio")))]
        {
            Err(MoaError::MissingEnvironmentVariable(service.to_string()))
        }
    }

    async fn set(&self, _service: &str, _scope: &str, _cred: Credential) -> Result<()> {
        Err(MoaError::StorageError(
            "environment delivery credential vault is read-only".to_string(),
        ))
    }

    async fn delete(&self, _service: &str, _scope: &str) -> Result<bool> {
        Err(MoaError::StorageError(
            "environment delivery credential vault is read-only".to_string(),
        ))
    }

    async fn list(&self, _service_prefix: &str) -> Result<Vec<StoredCredentialMetadata>> {
        Err(MoaError::StorageError(
            "environment delivery credential vault does not support listing".to_string(),
        ))
    }
}

#[cfg(feature = "postmark")]
async fn optional_postmark_client(
    vault: Arc<dyn CredentialVault>,
    scope: &str,
    config: &MessagingConfig,
) -> Result<Option<PostmarkEmailClient>> {
    match PostmarkEmailClient::from_vault(vault, scope, config).await {
        Ok(client) => Ok(Some(client)),
        Err(MoaError::MissingEnvironmentVariable(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(feature = "twilio")]
async fn optional_twilio_client(
    vault: Arc<dyn CredentialVault>,
    scope: &str,
    config: &MessagingConfig,
) -> Result<Option<TwilioSmsClient>> {
    match TwilioSmsClient::from_vault(vault, scope, config).await {
        Ok(client) => Ok(Some(client)),
        Err(MoaError::MissingEnvironmentVariable(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(any(feature = "postmark", feature = "twilio"))]
fn required_env(name: &str) -> Result<String> {
    optional_env(name).ok_or_else(|| MoaError::MissingEnvironmentVariable(name.to_string()))
}

#[cfg(any(feature = "postmark", feature = "twilio"))]
fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn delivery_span(message: &DeliveryMessage) -> tracing::Span {
    tracing::info_span!(
        "contact_delivery",
        otel.name = "contact_delivery_send",
        messaging.operation = "deliver",
        messaging.channel = message.channel.as_str(),
        moa.tenant.id = %message.tenant_id,
        moa.contact.id = %message.contact_id,
        moa.delivery.purpose = message.purpose.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use moa_core::{Channel, ContactId};
    use uuid::Uuid;

    use super::{DeliveryMessage, DeliveryPurpose};

    #[test]
    fn contact_verification_otp_builds_channel_specific_message() {
        // Pins: OTP rendering includes the code only in the provider payload, not metadata.
        let tenant_id = Uuid::now_v7();
        let contact_id = ContactId::new();
        let expires_at = chrono::Utc
            .with_ymd_and_hms(2026, 6, 21, 12, 0, 0)
            .single()
            .expect("fixed timestamp should be valid");

        let message = DeliveryMessage::contact_verification_otp(
            tenant_id,
            contact_id,
            Channel::Sms,
            "+15005550006",
            "123456",
            expires_at,
        );

        assert_eq!(message.tenant_id, tenant_id);
        assert_eq!(message.contact_id, contact_id);
        assert_eq!(message.purpose, DeliveryPurpose::ContactVerification);
        assert_eq!(message.channel, Channel::Sms);
        assert_eq!(message.to, "+15005550006");
        assert!(message.text_body.contains("123456"));
        assert_eq!(
            message.metadata.get("purpose").map(String::as_str),
            Some("contact_verification")
        );
        assert!(
            !message
                .metadata
                .values()
                .any(|value| value.contains("123456")),
            "verification code must not be copied into provider metadata"
        );
    }
}
