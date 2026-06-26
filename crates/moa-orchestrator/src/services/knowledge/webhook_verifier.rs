//! Provider and parser webhook verification adapters for the Knowledge service.

use std::sync::Arc;

use async_trait::async_trait;
use moa_knowledge::{
    domain::WebhookEvent,
    parser::{map_parser_webhook, verify_parser_webhook},
    providers::LinkedIntegrationProvider,
};
use reqwest::header::{HeaderMap, HeaderName};
use tokio_util::bytes::Bytes;

/// Verifies one raw provider webhook and normalizes its safe event metadata.
#[async_trait]
pub trait KnowledgeWebhookVerifier: Send + Sync {
    /// Verifies the raw webhook request and returns a normalized event.
    async fn verify_webhook(
        &self,
        headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent>;
}

#[derive(Clone)]
pub(super) struct LinkedProviderWebhookVerifier {
    provider: Arc<dyn LinkedIntegrationProvider>,
}

impl LinkedProviderWebhookVerifier {
    pub(super) fn new(provider: Arc<dyn LinkedIntegrationProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl KnowledgeWebhookVerifier for LinkedProviderWebhookVerifier {
    async fn verify_webhook(
        &self,
        headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        self.provider.verify_webhook(headers, body).await
    }
}

/// HMAC and custom-header verifier for parser-origin webhooks.
#[derive(Debug, Clone)]
pub struct ParserWebhookVerifier {
    provider: String,
    signing_key: Option<String>,
    custom_header: Option<(String, String)>,
}

impl ParserWebhookVerifier {
    /// Creates a parser webhook verifier for a stable provider identifier.
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            signing_key: None,
            custom_header: None,
        }
    }

    /// Requires an HMAC-SHA256 signature header for webhook verification.
    #[must_use]
    pub fn with_signing_key(mut self, signing_key: impl Into<String>) -> Self {
        self.signing_key = Some(signing_key.into());
        self
    }

    /// Requires an exact custom header name and value for webhook verification.
    #[must_use]
    pub fn with_custom_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom_header = Some((name.into(), value.into()));
        self
    }

    fn verify_configured(&self) -> moa_knowledge::Result<()> {
        if self.signing_key.is_some() || self.custom_header.is_some() {
            return Ok(());
        }
        Err(moa_knowledge::Error::Config(format!(
            "{} webhook verifier requires a signing key or custom header",
            self.provider
        )))
    }

    fn verify_custom_header(&self, headers: &HeaderMap) -> moa_knowledge::Result<()> {
        let Some((name, expected_value)) = self.custom_header.as_ref() else {
            return Ok(());
        };
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            moa_knowledge::Error::provider(
                &self.provider,
                format!("webhook custom header name `{name}` failed: {error}"),
            )
        })?;
        let actual = headers.get(&header_name).ok_or_else(|| {
            moa_knowledge::Error::provider(
                &self.provider,
                format!("webhook missing `{name}` header"),
            )
        })?;
        let actual = actual.to_str().map_err(|error| {
            moa_knowledge::Error::provider(
                &self.provider,
                format!("webhook header `{name}` failed: {error}"),
            )
        })?;
        if actual == expected_value {
            return Ok(());
        }
        Err(moa_knowledge::Error::provider(
            &self.provider,
            format!("webhook header `{name}` verification failed"),
        ))
    }
}

#[async_trait]
impl KnowledgeWebhookVerifier for ParserWebhookVerifier {
    async fn verify_webhook(
        &self,
        headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        self.verify_configured()?;
        self.verify_custom_header(&headers)?;
        if let Some(signing_key) = self.signing_key.as_deref() {
            return verify_parser_webhook(&self.provider, &headers, &body, signing_key);
        }
        map_parser_webhook(&self.provider, &headers, &body)
    }
}
