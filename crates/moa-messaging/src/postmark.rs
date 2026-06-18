//! Postmark email notification connector.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::{Credential, CredentialVault, MessagingConfig, MoaError, Result};
use reqwest::{
    StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, field};

const POSTMARK_EMAIL_PATH: &str = "/email";
const DEFAULT_RATE_LIMIT_RETRIES: usize = 3;
const DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(1);

/// Credential service key used for Postmark server tokens in `CredentialVault`.
pub const POSTMARK_SERVER_TOKEN_SERVICE: &str = "platform.postmark.server_token";

/// Local environment variable used by live Postmark tests for the server token.
pub const POSTMARK_SERVER_API_TOKEN_ENV: &str = "POSTMARK_SERVER_API_TOKEN";

/// Postmark's non-delivery server token for validating email API payloads.
pub const POSTMARK_TEST_TOKEN: &str = "POSTMARK_API_TEST";

/// Outbound email message accepted by the Postmark connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostmarkEmailMessage {
    /// Verified sender address, optionally with a display name.
    pub from: String,
    /// Primary recipients.
    pub to: Vec<String>,
    /// Carbon-copy recipients.
    pub cc: Vec<String>,
    /// Blind-carbon-copy recipients.
    pub bcc: Vec<String>,
    /// Email subject.
    pub subject: String,
    /// Plain-text body.
    pub text_body: Option<String>,
    /// HTML body.
    pub html_body: Option<String>,
    /// Optional reply-to address.
    pub reply_to: Option<String>,
    /// Optional Postmark tag.
    pub tag: Option<String>,
    /// Optional Postmark message stream. Falls back to the client default when omitted.
    pub message_stream: Option<String>,
    /// Optional Postmark metadata values.
    pub metadata: BTreeMap<String, String>,
}

impl PostmarkEmailMessage {
    /// Creates a new email message with one recipient and no body.
    pub fn new(from: impl Into<String>, to: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: vec![to.into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.into(),
            text_body: None,
            html_body: None,
            reply_to: None,
            tag: None,
            message_stream: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Adds a plain-text body.
    #[must_use]
    pub fn with_text_body(mut self, body: impl Into<String>) -> Self {
        self.text_body = Some(body.into());
        self
    }

    /// Adds an HTML body.
    #[must_use]
    pub fn with_html_body(mut self, body: impl Into<String>) -> Self {
        self.html_body = Some(body.into());
        self
    }

    /// Adds a carbon-copy recipient.
    #[must_use]
    pub fn with_cc(mut self, recipient: impl Into<String>) -> Self {
        self.cc.push(recipient.into());
        self
    }

    /// Adds a blind-carbon-copy recipient.
    #[must_use]
    pub fn with_bcc(mut self, recipient: impl Into<String>) -> Self {
        self.bcc.push(recipient.into());
        self
    }

    /// Adds a reply-to address.
    #[must_use]
    pub fn with_reply_to(mut self, recipient: impl Into<String>) -> Self {
        self.reply_to = Some(recipient.into());
        self
    }

    /// Sets the Postmark tag.
    #[must_use]
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Sets the Postmark message stream for this message.
    #[must_use]
    pub fn with_message_stream(mut self, stream: impl Into<String>) -> Self {
        self.message_stream = Some(stream.into());
        self
    }

    /// Adds one Postmark metadata key/value pair.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Result returned by Postmark after accepting an email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostmarkEmailSendResult {
    /// Recipient summary returned by Postmark.
    pub to: String,
    /// Submission timestamp returned by Postmark when present.
    pub submitted_at: Option<DateTime<Utc>>,
    /// Postmark message identifier.
    pub message_id: String,
    /// Postmark error code. Zero indicates success.
    pub error_code: i64,
    /// Human-readable Postmark response message.
    pub message: String,
}

impl PostmarkEmailSendResult {
    /// Returns Postmark API failure details when `ErrorCode` reports a send rejection.
    pub fn send_failure(&self) -> Option<PostmarkEmailFailure> {
        classify_postmark_failure(self.error_code, &self.message)
    }

    /// Returns true when Postmark accepted the email for delivery processing.
    pub fn is_accepted(&self) -> bool {
        self.error_code == 0 && !self.message_id.trim().is_empty()
    }
}

/// Retry classification for a Postmark email send failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostmarkEmailFailureClass {
    /// A later send may succeed after the referenced transient condition clears.
    Retryable,
    /// A retry is not expected to help without configuration, account, or recipient changes.
    Permanent,
}

impl PostmarkEmailFailureClass {
    /// Returns the stable telemetry label for this failure class.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::Permanent => "permanent",
        }
    }
}

/// Structured failure details returned by Postmark when an email is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostmarkEmailFailure {
    /// Postmark API error code.
    pub error_code: i64,
    /// Postmark API error message.
    pub error_message: String,
    /// Retry classification for this failure.
    pub class: PostmarkEmailFailureClass,
    /// Suggested delay before a durable caller attempts another send.
    pub backoff_hint: Option<Duration>,
    /// Human-readable reason safe for logs and operator UI.
    pub reason: String,
}

impl PostmarkEmailFailure {
    /// Returns whether the send failure can be retried by a durable caller.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.class == PostmarkEmailFailureClass::Retryable
    }
}

/// Async Postmark email API client.
#[derive(Clone)]
pub struct PostmarkEmailClient {
    client: reqwest::Client,
    server_token: SecretString,
    base_url: String,
    default_message_stream: Option<String>,
    max_rate_limit_retries: usize,
    rate_limit_backoff: Duration,
}

impl PostmarkEmailClient {
    /// Creates a Postmark client from a server token.
    pub fn new(server_token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            server_token: SecretString::from(server_token.into()),
            base_url: "https://api.postmarkapp.com".to_string(),
            default_message_stream: Some("outbound".to_string()),
            max_rate_limit_retries: DEFAULT_RATE_LIMIT_RETRIES,
            rate_limit_backoff: DEFAULT_RATE_LIMIT_BACKOFF,
        }
    }

    /// Creates a Postmark client from a configured credential vault.
    pub async fn from_vault(
        vault: Arc<dyn CredentialVault>,
        scope: &str,
        config: &MessagingConfig,
    ) -> Result<Self> {
        let credential = vault.get(POSTMARK_SERVER_TOKEN_SERVICE, scope).await?;
        let token = postmark_token_from_credential(credential)?;
        Ok(Self::new(token)
            .with_base_url(config.postmark_base_url.clone())
            .with_default_message_stream(config.postmark_message_stream.clone()))
    }

    /// Overrides the HTTP client, primarily for tests.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Overrides the Postmark API base URL, primarily for tests.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Overrides the default Postmark message stream.
    #[must_use]
    pub fn with_default_message_stream(mut self, stream: impl Into<String>) -> Self {
        self.default_message_stream = Some(stream.into());
        self
    }

    /// Overrides the number of safe 429 retries before surfacing rate-limit failure.
    #[must_use]
    pub fn with_max_rate_limit_retries(mut self, max_retries: usize) -> Self {
        self.max_rate_limit_retries = max_retries;
        self
    }

    /// Overrides the fallback delay used when Postmark omits `Retry-After` on a 429 response.
    #[must_use]
    pub fn with_rate_limit_backoff(mut self, backoff: Duration) -> Self {
        self.rate_limit_backoff = backoff;
        self
    }

    /// Sends one email through Postmark's `/email` endpoint.
    pub async fn send_email(
        &self,
        message: &PostmarkEmailMessage,
    ) -> Result<PostmarkEmailSendResult> {
        async {
            let request = message.to_request(self.default_message_stream.as_deref())?;
            if let Some(message_stream) = request.message_stream.as_deref() {
                tracing::Span::current().record("postmark.message_stream", message_stream);
            }
            let url = self.email_url();
            self.send_postmark_request(|| {
                self.client
                    .post(&url)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json")
                    .header("X-Postmark-Server-Token", self.server_token.expose_secret())
                    .json(&request)
            })
            .await
        }
        .instrument(postmark_span("postmark_email_send", "send_email"))
        .await
    }

    fn email_url(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            POSTMARK_EMAIL_PATH
        )
    }

    async fn send_postmark_request<F>(
        &self,
        mut build_request: F,
    ) -> Result<PostmarkEmailSendResult>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut retries = 0usize;
        loop {
            let response = match build_request().send().await {
                Ok(response) => response,
                Err(error) => {
                    let message = error.to_string();
                    record_postmark_api_error(None, None, None, false, &message);
                    tracing::warn!(
                        messaging.system = "postmark",
                        messaging.operation = "send_email",
                        error = %message,
                        retryable = false,
                        "postmark email API request failed before an HTTP response"
                    );
                    return Err(MoaError::ProviderError(message));
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            let body = response_text(response).await;

            if status == StatusCode::TOO_MANY_REQUESTS && retries < self.max_rate_limit_retries {
                let delay = retry_after_delay(&headers).unwrap_or(self.rate_limit_backoff);
                record_postmark_api_error(
                    Some(status),
                    parse_postmark_error_code(&body),
                    Some(delay),
                    true,
                    &body,
                );
                tracing::warn!(
                    messaging.system = "postmark",
                    messaging.operation = "send_email",
                    http.status_code = status.as_u16(),
                    retry_after_ms = delay.as_millis() as u64,
                    attempt = retries + 1,
                    max_retries = self.max_rate_limit_retries,
                    "postmark email API request was rate limited; retrying"
                );
                retries += 1;
                tokio::time::sleep(delay).await;
                continue;
            }

            return decode_postmark_response(status, &headers, body, retries);
        }
    }
}

fn decode_postmark_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: String,
    retries: usize,
) -> Result<PostmarkEmailSendResult> {
    if !status.is_success() {
        let retry_after = retry_after_delay(headers);
        let retryable = is_retryable_http_status(status);
        record_postmark_api_error(
            Some(status),
            parse_postmark_error_code(&body),
            retry_after,
            retryable,
            &body,
        );
        tracing::warn!(
            messaging.system = "postmark",
            http.status_code = status.as_u16(),
            retryable,
            error = %body,
            "postmark email API returned a non-success status"
        );
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(MoaError::RateLimited {
                retries,
                message: body,
            });
        }
        return Err(MoaError::HttpStatus {
            status: status.as_u16(),
            retry_after,
            message: body,
        });
    }

    let result = serde_json::from_str::<PostmarkEmailResponse>(&body)
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
    let result = PostmarkEmailSendResult::from(result);
    record_postmark_result(&result);
    if let Some(failure) = result.send_failure() {
        return Err(postmark_failure_error(&failure));
    }
    Ok(result)
}

async fn response_text(response: reqwest::Response) -> String {
    response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read response body: {error}"))
}

fn postmark_span(name: &'static str, operation: &'static str) -> tracing::Span {
    tracing::info_span!(
        "postmark_email",
        otel.name = name,
        messaging.system = "postmark",
        messaging.operation = operation,
        messaging.channel = "email",
        postmark.message_id = field::Empty,
        postmark.error_code = field::Empty,
        postmark.failure_class = field::Empty,
        postmark.retryable = field::Empty,
        postmark.retry_after_ms = field::Empty,
        postmark.message_stream = field::Empty,
        http.status_code = field::Empty,
        error = field::Empty,
    )
}

fn record_postmark_result(result: &PostmarkEmailSendResult) {
    let span = tracing::Span::current();
    span.record("postmark.message_id", result.message_id.as_str());
    span.record("postmark.error_code", result.error_code);
    if let Some(failure) = result.send_failure() {
        span.record("postmark.failure_class", failure.class.label());
        span.record("postmark.retryable", failure.is_retryable());
        if let Some(retry_after) = failure.backoff_hint {
            span.record("postmark.retry_after_ms", retry_after.as_millis() as u64);
        }
        span.record("error", failure.reason.as_str());
        tracing::error!(
            messaging.system = "postmark",
            postmark.message_id = %result.message_id,
            postmark.error_code = failure.error_code,
            postmark.failure_class = failure.class.label(),
            postmark.retryable = failure.is_retryable(),
            error = %failure.reason,
            "postmark email send was rejected by API response"
        );
    }
}

fn record_postmark_api_error(
    status: Option<StatusCode>,
    error_code: Option<i64>,
    retry_after: Option<Duration>,
    retryable: bool,
    message: &str,
) {
    let span = tracing::Span::current();
    if let Some(status) = status {
        span.record("http.status_code", status.as_u16());
    }
    if let Some(error_code) = error_code {
        span.record("postmark.error_code", error_code);
    }
    if let Some(retry_after) = retry_after {
        span.record("postmark.retry_after_ms", retry_after.as_millis() as u64);
    }
    let failure_class = if retryable {
        PostmarkEmailFailureClass::Retryable
    } else {
        PostmarkEmailFailureClass::Permanent
    };
    span.record("postmark.failure_class", failure_class.label());
    span.record("postmark.retryable", retryable);
    span.record("error", message);
}

fn postmark_failure_error(failure: &PostmarkEmailFailure) -> MoaError {
    let message = format!(
        "postmark email {} failure ErrorCode {}: {}",
        failure.class.label(),
        failure.error_code,
        failure.error_message
    );
    match failure.class {
        PostmarkEmailFailureClass::Retryable => MoaError::ProviderQuirk(message),
        PostmarkEmailFailureClass::Permanent => MoaError::ProviderError(message),
    }
}

fn is_retryable_http_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value)
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let reset_at = DateTime::parse_from_rfc2822(value).ok()?;
    let reset_at = reset_at.with_timezone(&Utc);
    let remaining = reset_at.signed_duration_since(Utc::now());
    let millis = remaining.num_milliseconds().max(0) as u64;
    Some(Duration::from_millis(millis))
}

fn parse_postmark_error_code(body: &str) -> Option<i64> {
    serde_json::from_str::<PostmarkApiErrorResponse>(body)
        .ok()
        .and_then(|error| error.error_code)
}

fn classify_postmark_failure(error_code: i64, error_message: &str) -> Option<PostmarkEmailFailure> {
    if error_code == 0 {
        return None;
    }

    let (class, backoff_hint, reason) = match error_code {
        100 => (
            PostmarkEmailFailureClass::Retryable,
            Some(Duration::from_secs(300)),
            "postmark API is offline for maintenance",
        ),
        429 => (
            PostmarkEmailFailureClass::Retryable,
            Some(Duration::from_secs(60)),
            "postmark email API rate limit was exceeded",
        ),
        406 => (
            PostmarkEmailFailureClass::Permanent,
            None,
            "recipient is inactive in Postmark suppression state",
        ),
        405 => (
            PostmarkEmailFailureClass::Permanent,
            None,
            "postmark account has run out of credits",
        ),
        400 | 401 => (
            PostmarkEmailFailureClass::Permanent,
            None,
            "postmark sender signature is missing or unconfirmed",
        ),
        412 | 413 => (
            PostmarkEmailFailureClass::Permanent,
            None,
            "postmark account is not approved for this send",
        ),
        _ => (
            PostmarkEmailFailureClass::Permanent,
            None,
            "postmark email send failed with a nonzero API error code",
        ),
    };

    Some(PostmarkEmailFailure {
        error_code,
        error_message: error_message.to_string(),
        class,
        backoff_hint,
        reason: reason.to_string(),
    })
}

impl PostmarkEmailMessage {
    fn to_request(&self, default_message_stream: Option<&str>) -> Result<PostmarkEmailRequest> {
        let from = required_field("from", &self.from)?;
        let subject = required_field("subject", &self.subject)?;
        let to = recipients("to", &self.to)?;
        let text_body = self.text_body.as_deref().and_then(optional_field);
        let html_body = self.html_body.as_deref().and_then(optional_field);
        if text_body.is_none() && html_body.is_none() {
            return Err(MoaError::ValidationError(
                "postmark email requires text_body or html_body".to_string(),
            ));
        }

        Ok(PostmarkEmailRequest {
            from,
            to,
            cc: optional_recipients(&self.cc),
            bcc: optional_recipients(&self.bcc),
            subject,
            text_body,
            html_body,
            reply_to: self.reply_to.as_deref().and_then(optional_field),
            tag: self.tag.as_deref().and_then(optional_field),
            message_stream: self
                .message_stream
                .as_deref()
                .and_then(optional_field)
                .or_else(|| default_message_stream.and_then(optional_field)),
            metadata: if self.metadata.is_empty() {
                None
            } else {
                Some(self.metadata.clone())
            },
        })
    }
}

fn postmark_token_from_credential(credential: Credential) -> Result<String> {
    match credential {
        Credential::Bearer(token) => Ok(token),
        Credential::ApiKey { value, .. } => Ok(value),
        Credential::OAuth { .. } => Err(MoaError::ConfigError(
            "postmark server token credential must be bearer or api_key".to_string(),
        )),
    }
}

fn required_field(name: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(MoaError::ValidationError(format!(
            "postmark email {name} is required"
        )));
    }
    Ok(trimmed.to_string())
}

fn optional_field(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn recipients(name: &str, values: &[String]) -> Result<String> {
    let recipients = values
        .iter()
        .filter_map(|value| optional_field(value))
        .collect::<Vec<_>>();
    if recipients.is_empty() {
        return Err(MoaError::ValidationError(format!(
            "postmark email {name} recipient is required"
        )));
    }
    Ok(recipients.join(", "))
}

fn optional_recipients(values: &[String]) -> Option<String> {
    let recipients = values
        .iter()
        .filter_map(|value| optional_field(value))
        .collect::<Vec<_>>();
    if recipients.is_empty() {
        None
    } else {
        Some(recipients.join(", "))
    }
}

#[derive(Debug, Serialize)]
struct PostmarkEmailRequest {
    #[serde(rename = "From")]
    from: String,
    #[serde(rename = "To")]
    to: String,
    #[serde(rename = "Cc", skip_serializing_if = "Option::is_none")]
    cc: Option<String>,
    #[serde(rename = "Bcc", skip_serializing_if = "Option::is_none")]
    bcc: Option<String>,
    #[serde(rename = "Subject")]
    subject: String,
    #[serde(rename = "TextBody", skip_serializing_if = "Option::is_none")]
    text_body: Option<String>,
    #[serde(rename = "HtmlBody", skip_serializing_if = "Option::is_none")]
    html_body: Option<String>,
    #[serde(rename = "ReplyTo", skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
    #[serde(rename = "Tag", skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(rename = "MessageStream", skip_serializing_if = "Option::is_none")]
    message_stream: Option<String>,
    #[serde(rename = "Metadata", skip_serializing_if = "Option::is_none")]
    metadata: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct PostmarkEmailResponse {
    #[serde(rename = "To")]
    to: String,
    #[serde(rename = "SubmittedAt")]
    submitted_at: Option<DateTime<Utc>>,
    #[serde(rename = "MessageID")]
    message_id: String,
    #[serde(rename = "ErrorCode")]
    error_code: i64,
    #[serde(rename = "Message")]
    message: String,
}

#[derive(Debug, Deserialize)]
struct PostmarkApiErrorResponse {
    #[serde(rename = "ErrorCode", alias = "error_code", default)]
    error_code: Option<i64>,
}

impl From<PostmarkEmailResponse> for PostmarkEmailSendResult {
    fn from(value: PostmarkEmailResponse) -> Self {
        Self {
            to: value.to,
            submitted_at: value.submitted_at,
            message_id: value.message_id,
            error_code: value.error_code,
            message: value.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_request_requires_a_body() {
        // Pins: Postmark sends must include at least one body variant.
        let message = PostmarkEmailMessage::new("moa@example.com", "ops@example.com", "Alert");
        let error = message
            .to_request(Some("outbound"))
            .expect_err("missing body should be rejected before HTTP send");
        assert!(matches!(error, MoaError::ValidationError(_)));
    }

    #[test]
    fn email_request_joins_recipients_and_applies_default_stream() {
        // Pins: multi-recipient fields use Postmark's comma-separated JSON string shape.
        let message = PostmarkEmailMessage::new("moa@example.com", "ops@example.com", "Alert")
            .with_cc("audit@example.com")
            .with_text_body("body");
        let request = message
            .to_request(Some("alerts"))
            .expect("valid email should build a Postmark request");

        assert_eq!(request.from, "moa@example.com");
        assert_eq!(request.to, "ops@example.com");
        assert_eq!(request.cc.as_deref(), Some("audit@example.com"));
        assert_eq!(request.message_stream.as_deref(), Some("alerts"));
    }
}
