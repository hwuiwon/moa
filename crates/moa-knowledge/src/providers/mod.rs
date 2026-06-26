//! Linked-account provider traits and adapters.

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::HeaderMap;

use crate::{
    domain::{
        CreateLinkTokenRequest, ExchangePublicTokenRequest, LinkToken, LinkedAccount,
        ListChangedRecordsRequest, RecordPage, TriggerSyncRequest, TriggeredSync, WebhookEvent,
    },
    error::Result,
};

pub mod merge;
pub mod nango;

/// Tenant knowledge linked-account provider seam.
#[async_trait]
pub trait LinkedIntegrationProvider: Send + Sync {
    /// Creates a short-lived link token or hosted link URL.
    async fn create_link_token(&self, req: CreateLinkTokenRequest) -> Result<LinkToken>;

    /// Exchanges a public token for a linked account reference.
    async fn exchange_public_token(&self, req: ExchangePublicTokenRequest)
    -> Result<LinkedAccount>;

    /// Triggers a provider-side sync for one connection.
    async fn trigger_sync(&self, req: TriggerSyncRequest) -> Result<TriggeredSync>;

    /// Lists changed source records from the provider cache or API.
    async fn list_changed_records(&self, req: ListChangedRecordsRequest) -> Result<RecordPage>;

    /// Verifies and normalizes a provider webhook.
    async fn verify_webhook(&self, headers: HeaderMap, body: Bytes) -> Result<WebhookEvent>;
}

pub(crate) mod http {
    //! Shared HTTP helpers for knowledge provider and parser adapters.

    use std::time::Duration;

    use reqwest::{Client, Response, StatusCode, header::RETRY_AFTER};
    use serde::de::DeserializeOwned;

    use crate::error::{Error, Result};

    /// Builds the shared HTTP client used by knowledge adapters.
    pub(crate) fn build_http_client() -> Result<Client> {
        Client::builder()
            .user_agent(concat!("moa-knowledge/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| Error::Transport(format!("failed to build HTTP client: {error}")))
    }

    /// Reads a successful response as JSON or maps an error status.
    pub(crate) async fn json_response<T>(response: Response) -> Result<T>
    where
        T: DeserializeOwned,
    {
        ensure_success(response)
            .await?
            .json::<T>()
            .await
            .map_err(|error| Error::Decode(format!("failed to decode JSON response: {error}")))
    }

    /// Returns the response when it has a success status.
    pub(crate) async fn ensure_success(response: Response) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }

        let retry_after = retry_after_delay(status, response.headers().get(RETRY_AFTER));
        let message = response
            .text()
            .await
            .unwrap_or_else(|error| format!("response body could not be read: {error}"));
        Err(Error::HttpStatus {
            status: status.as_u16(),
            retry_after,
            message,
        })
    }

    fn retry_after_delay(
        status: StatusCode,
        value: Option<&reqwest::header::HeaderValue>,
    ) -> Option<Duration> {
        if status != StatusCode::TOO_MANY_REQUESTS {
            return None;
        }
        let seconds = value?.to_str().ok()?.trim().parse::<u64>().ok()?;
        Some(Duration::from_secs(seconds))
    }
}
