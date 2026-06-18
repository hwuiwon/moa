//! Twilio SMS notification connector.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::{Credential, CredentialVault, MessagingConfig, MoaError, Result};
use reqwest::{
    StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tracing::{Instrument, field};

const TWILIO_MESSAGES_PATH_PREFIX: &str = "/2010-04-01/Accounts/";
const DEFAULT_RATE_LIMIT_RETRIES: usize = 3;
const DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(1);

/// Credential service key for the Twilio account SID.
pub const TWILIO_ACCOUNT_SID_SERVICE: &str = "platform.twilio.account_sid";
/// Credential service key for the Twilio account auth token.
pub const TWILIO_AUTH_TOKEN_SERVICE: &str = "platform.twilio.auth_token";
/// Credential service key for the Twilio API key SID.
pub const TWILIO_API_KEY_SID_SERVICE: &str = "platform.twilio.api_key_sid";
/// Credential service key for the Twilio API key secret.
pub const TWILIO_API_KEY_SECRET_SERVICE: &str = "platform.twilio.api_key_secret";
/// Credential service key for the default Twilio sender phone number.
pub const TWILIO_FROM_NUMBER_SERVICE: &str = "platform.twilio.from_number";
/// Credential service key for the default Twilio Messaging Service SID.
pub const TWILIO_MESSAGING_SERVICE_SID_SERVICE: &str = "platform.twilio.messaging_service_sid";

/// Local environment variable for the Twilio account SID.
pub const TWILIO_ACCOUNT_SID_ENV: &str = "TWILIO_ACCOUNT_SID";
/// Legacy local environment variable commonly used for the Twilio account SID.
pub const TWILIO_SID_ENV: &str = "TWILIO_SID";
/// Local environment variable for the Twilio account auth token.
pub const TWILIO_AUTH_TOKEN_ENV: &str = "TWILIO_AUTH_TOKEN";
/// Local environment variable for the Twilio API key SID.
pub const TWILIO_API_KEY_SID_ENV: &str = "TWILIO_API_KEY_SID";
/// Local environment variable for the Twilio API key secret.
pub const TWILIO_API_KEY_SECRET_ENV: &str = "TWILIO_API_KEY_SECRET";
/// Local environment variable for the default Twilio sender phone number.
pub const TWILIO_FROM_NUMBER_ENV: &str = "TWILIO_FROM_NUMBER";
/// Local environment variable for the default Twilio Messaging Service SID.
pub const TWILIO_MESSAGING_SERVICE_SID_ENV: &str = "TWILIO_MESSAGING_SERVICE_SID";

/// Outbound SMS message accepted by the Twilio connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwilioSmsMessage {
    /// Recipient phone number in E.164 format.
    pub to: String,
    /// SMS body.
    pub body: String,
    /// Optional sender phone number.
    pub from: Option<String>,
    /// Optional Twilio Messaging Service SID.
    pub messaging_service_sid: Option<String>,
    /// Optional status callback URL.
    pub status_callback: Option<String>,
}

impl TwilioSmsMessage {
    /// Creates a new SMS message without a sender override.
    pub fn new(to: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            to: to.into(),
            body: body.into(),
            from: None,
            messaging_service_sid: None,
            status_callback: None,
        }
    }

    /// Sets the Twilio sender phone number for this message.
    #[must_use]
    pub fn with_from(mut self, from: impl Into<String>) -> Self {
        self.from = Some(from.into());
        self
    }

    /// Sets the Twilio Messaging Service SID for this message.
    #[must_use]
    pub fn with_messaging_service_sid(mut self, sid: impl Into<String>) -> Self {
        self.messaging_service_sid = Some(sid.into());
        self
    }

    /// Sets a Twilio status callback URL for this message.
    #[must_use]
    pub fn with_status_callback(mut self, url: impl Into<String>) -> Self {
        self.status_callback = Some(url.into());
        self
    }
}

/// Result returned by Twilio after accepting an SMS send request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwilioSmsSendResult {
    /// Twilio message SID.
    pub sid: String,
    /// Twilio message status.
    pub status: String,
    /// Recipient phone number returned by Twilio.
    pub to: String,
    /// Sender phone number returned by Twilio, if assigned.
    pub from: Option<String>,
    /// Messaging Service SID returned by Twilio, if assigned.
    pub messaging_service_sid: Option<String>,
    /// Twilio error code when the message has failed or is undelivered.
    pub error_code: Option<i64>,
    /// Twilio error message when the message has failed or is undelivered.
    pub error_message: Option<String>,
    /// Twilio resource URI.
    pub uri: String,
}

impl TwilioSmsSendResult {
    /// Returns delivery failure details when Twilio reports a terminal failed state.
    pub fn delivery_failure(&self) -> Option<TwilioSmsDeliveryFailure> {
        classify_delivery_failure(&self.status, self.error_code, self.error_message.as_deref())
    }

    /// Returns true when Twilio has confirmed the SMS reached a carrier or handset.
    pub fn is_handed_off_or_delivered(&self) -> bool {
        matches!(self.status.as_str(), "sent" | "delivered" | "read")
    }
}

/// Retry classification for a Twilio SMS delivery failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwilioSmsFailureClass {
    /// A later send may succeed after the referenced transient condition clears.
    Retryable,
    /// A retry is not expected to help without configuration, recipient, or compliance changes.
    Permanent,
}

impl TwilioSmsFailureClass {
    /// Returns the stable telemetry label for this failure class.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::Permanent => "permanent",
        }
    }
}

/// Structured delivery failure details returned by Twilio after message acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwilioSmsDeliveryFailure {
    /// Twilio terminal message status, usually `failed`, `undelivered`, or `canceled`.
    pub status: String,
    /// Twilio delivery error code when available.
    pub error_code: Option<i64>,
    /// Twilio delivery error message when available.
    pub error_message: Option<String>,
    /// Retry classification for this delivery failure.
    pub class: TwilioSmsFailureClass,
    /// Suggested delay before a caller attempts another send.
    pub backoff_hint: Option<Duration>,
    /// Human-readable reason safe for logs and operator UI.
    pub reason: String,
}

impl TwilioSmsDeliveryFailure {
    /// Returns whether the delivery failure can be retried by a durable caller.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.class == TwilioSmsFailureClass::Retryable
    }
}

/// Async Twilio SMS API client.
#[derive(Clone)]
pub struct TwilioSmsClient {
    client: reqwest::Client,
    account_sid: String,
    username: String,
    password: SecretString,
    base_url: String,
    default_from: Option<String>,
    default_messaging_service_sid: Option<String>,
    max_rate_limit_retries: usize,
    rate_limit_backoff: Duration,
}

impl TwilioSmsClient {
    /// Creates a Twilio client from explicit basic-auth credentials.
    pub fn new(
        account_sid: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            account_sid: account_sid.into(),
            username: username.into(),
            password: SecretString::from(password.into()),
            base_url: "https://api.twilio.com".to_string(),
            default_from: None,
            default_messaging_service_sid: None,
            max_rate_limit_retries: DEFAULT_RATE_LIMIT_RETRIES,
            rate_limit_backoff: DEFAULT_RATE_LIMIT_BACKOFF,
        }
    }

    /// Creates a Twilio client using an account SID and auth token.
    pub fn from_account_auth_token(
        account_sid: impl Into<String>,
        auth_token: impl Into<String>,
    ) -> Self {
        let account_sid = account_sid.into();
        Self::new(account_sid.clone(), account_sid, auth_token)
    }

    /// Creates a Twilio client using an API key SID and API key secret.
    pub fn from_api_key(
        account_sid: impl Into<String>,
        api_key_sid: impl Into<String>,
        api_key_secret: impl Into<String>,
    ) -> Self {
        Self::new(account_sid, api_key_sid, api_key_secret)
    }

    /// Creates a Twilio client from a configured credential vault.
    pub async fn from_vault(
        vault: Arc<dyn CredentialVault>,
        scope: &str,
        config: &MessagingConfig,
    ) -> Result<Self> {
        let account_sid = required_vault_string(&vault, TWILIO_ACCOUNT_SID_SERVICE, scope).await?;
        let api_key_sid = optional_vault_string(&vault, TWILIO_API_KEY_SID_SERVICE, scope).await?;
        let api_key_secret =
            optional_vault_string(&vault, TWILIO_API_KEY_SECRET_SERVICE, scope).await?;
        let mut client = match (api_key_sid, api_key_secret) {
            (Some(sid), Some(secret)) => Self::from_api_key(account_sid, sid, secret),
            (Some(_), None) => {
                return Err(MoaError::ConfigError(
                    "twilio api key sid requires twilio api key secret".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(MoaError::ConfigError(
                    "twilio api key secret requires twilio api key sid".to_string(),
                ));
            }
            (None, None) => {
                let auth_token =
                    required_vault_string(&vault, TWILIO_AUTH_TOKEN_SERVICE, scope).await?;
                Self::from_account_auth_token(account_sid, auth_token)
            }
        }
        .with_base_url(config.twilio_base_url.clone());

        if let Some(from) = optional_vault_string(&vault, TWILIO_FROM_NUMBER_SERVICE, scope).await?
        {
            client = client.with_default_from(from);
        }
        if let Some(messaging_service_sid) =
            optional_vault_string(&vault, TWILIO_MESSAGING_SERVICE_SID_SERVICE, scope).await?
        {
            client = client.with_default_messaging_service_sid(messaging_service_sid);
        }
        Ok(client)
    }

    /// Overrides the HTTP client, primarily for tests.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Overrides the Twilio API base URL, primarily for tests.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets the default Twilio sender phone number.
    #[must_use]
    pub fn with_default_from(mut self, from: impl Into<String>) -> Self {
        self.default_from = Some(from.into());
        self
    }

    /// Sets the default Twilio Messaging Service SID.
    #[must_use]
    pub fn with_default_messaging_service_sid(mut self, sid: impl Into<String>) -> Self {
        self.default_messaging_service_sid = Some(sid.into());
        self
    }

    /// Overrides the number of safe 429 retries before surfacing rate-limit failure.
    #[must_use]
    pub fn with_max_rate_limit_retries(mut self, max_retries: usize) -> Self {
        self.max_rate_limit_retries = max_retries;
        self
    }

    /// Overrides the fallback delay used when Twilio omits `Retry-After` on a 429 response.
    #[must_use]
    pub fn with_rate_limit_backoff(mut self, backoff: Duration) -> Self {
        self.rate_limit_backoff = backoff;
        self
    }

    /// Sends one SMS through Twilio's Messages API.
    pub async fn send_sms(&self, message: &TwilioSmsMessage) -> Result<TwilioSmsSendResult> {
        async {
            let request = message.to_request(
                self.default_from.as_deref(),
                self.default_messaging_service_sid.as_deref(),
            )?;
            tracing::Span::current().record(
                "twilio.messaging_service_sid",
                request.messaging_service_sid.as_deref().unwrap_or(""),
            );
            let form = request.to_form();
            let url = self.messages_url();
            self.send_twilio_request("send_sms", || {
                self.client
                    .post(&url)
                    .basic_auth(&self.username, Some(self.password.expose_secret()))
                    .form(&form)
            })
            .await
        }
        .instrument(twilio_span("twilio_sms_send", "send_sms"))
        .await
    }

    /// Fetches the latest Twilio status for a message SID.
    pub async fn fetch_sms(&self, message_sid: &str) -> Result<TwilioSmsSendResult> {
        async {
            let sid = required_field("message_sid", message_sid)?;
            tracing::Span::current().record("twilio.message_sid", sid.as_str());
            let url = self.message_url(&sid);
            self.send_twilio_request("fetch_sms", || {
                self.client
                    .get(&url)
                    .basic_auth(&self.username, Some(self.password.expose_secret()))
            })
            .await
        }
        .instrument(twilio_span("twilio_sms_fetch", "fetch_sms"))
        .await
    }

    fn messages_url(&self) -> String {
        format!(
            "{}{}{}/Messages.json",
            self.base_url.trim_end_matches('/'),
            TWILIO_MESSAGES_PATH_PREFIX,
            self.account_sid
        )
    }

    fn message_url(&self, message_sid: &str) -> String {
        format!(
            "{}{}{}/Messages/{}.json",
            self.base_url.trim_end_matches('/'),
            TWILIO_MESSAGES_PATH_PREFIX,
            self.account_sid,
            message_sid
        )
    }

    async fn send_twilio_request<F>(
        &self,
        operation: &'static str,
        mut build_request: F,
    ) -> Result<TwilioSmsSendResult>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut retries = 0usize;
        loop {
            let response = match build_request().send().await {
                Ok(response) => response,
                Err(error) => {
                    let message = error.to_string();
                    record_api_error(None, None, None, false, &message);
                    tracing::warn!(
                        messaging.system = "twilio",
                        messaging.operation = operation,
                        error = %message,
                        retryable = false,
                        "twilio sms api request failed before an HTTP response"
                    );
                    return Err(MoaError::ProviderError(message));
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            let body = response_text(response).await;

            if status == StatusCode::TOO_MANY_REQUESTS && retries < self.max_rate_limit_retries {
                let delay = retry_after_delay(&headers).unwrap_or(self.rate_limit_backoff);
                record_api_error(
                    Some(status),
                    parse_twilio_error_code(&body),
                    Some(delay),
                    true,
                    &body,
                );
                tracing::warn!(
                    messaging.system = "twilio",
                    messaging.operation = operation,
                    http.status_code = status.as_u16(),
                    retry_after_ms = delay.as_millis() as u64,
                    attempt = retries + 1,
                    max_retries = self.max_rate_limit_retries,
                    "twilio sms api request was rate limited; retrying"
                );
                retries += 1;
                tokio::time::sleep(delay).await;
                continue;
            }

            return decode_twilio_response(status, &headers, body, retries);
        }
    }
}

fn decode_twilio_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: String,
    retries: usize,
) -> Result<TwilioSmsSendResult> {
    if !status.is_success() {
        let retry_after = retry_after_delay(headers);
        let retryable = is_retryable_http_status(status);
        record_api_error(
            Some(status),
            parse_twilio_error_code(&body),
            retry_after,
            retryable,
            &body,
        );
        tracing::warn!(
            messaging.system = "twilio",
            http.status_code = status.as_u16(),
            retryable,
            error = %body,
            "twilio sms api returned a non-success status"
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

    let result = serde_json::from_str::<TwilioSmsResponse>(&body)
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
    let result = TwilioSmsSendResult::from(result);
    record_twilio_result(&result);
    Ok(result)
}

async fn response_text(response: reqwest::Response) -> String {
    response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read response body: {error}"))
}

fn twilio_span(name: &'static str, operation: &'static str) -> tracing::Span {
    tracing::info_span!(
        "twilio_sms",
        otel.name = name,
        messaging.system = "twilio",
        messaging.operation = operation,
        messaging.channel = "sms",
        twilio.message_sid = field::Empty,
        twilio.message_status = field::Empty,
        twilio.messaging_service_sid = field::Empty,
        twilio.error_code = field::Empty,
        twilio.failure_class = field::Empty,
        twilio.retryable = field::Empty,
        twilio.retry_after_ms = field::Empty,
        http.status_code = field::Empty,
        error = field::Empty,
    )
}

fn record_twilio_result(result: &TwilioSmsSendResult) {
    let span = tracing::Span::current();
    span.record("twilio.message_sid", result.sid.as_str());
    span.record("twilio.message_status", result.status.as_str());
    if let Some(messaging_service_sid) = &result.messaging_service_sid {
        span.record(
            "twilio.messaging_service_sid",
            messaging_service_sid.as_str(),
        );
    }
    if let Some(error_code) = result.error_code {
        span.record("twilio.error_code", error_code);
    }
    if let Some(failure) = result.delivery_failure() {
        span.record("twilio.failure_class", failure.class.label());
        span.record("twilio.retryable", failure.is_retryable());
        span.record("error", failure.reason.as_str());
        tracing::error!(
            messaging.system = "twilio",
            twilio.message_sid = %result.sid,
            twilio.message_status = %result.status,
            twilio.error_code = ?failure.error_code,
            twilio.failure_class = failure.class.label(),
            twilio.retryable = failure.is_retryable(),
            error = %failure.reason,
            "twilio sms reached a terminal delivery failure"
        );
    }
}

fn record_api_error(
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
        span.record("twilio.error_code", error_code);
    }
    if let Some(retry_after) = retry_after {
        span.record("twilio.retry_after_ms", retry_after.as_millis() as u64);
    }
    let failure_class = if retryable {
        TwilioSmsFailureClass::Retryable
    } else {
        TwilioSmsFailureClass::Permanent
    };
    span.record("twilio.failure_class", failure_class.label());
    span.record("twilio.retryable", retryable);
    span.record("error", message);
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

fn parse_twilio_error_code(body: &str) -> Option<i64> {
    serde_json::from_str::<TwilioApiErrorResponse>(body)
        .ok()
        .and_then(|error| error.code)
}

fn classify_delivery_failure(
    status: &str,
    error_code: Option<i64>,
    error_message: Option<&str>,
) -> Option<TwilioSmsDeliveryFailure> {
    if !matches!(status, "failed" | "undelivered" | "canceled") {
        return None;
    }

    let (class, backoff_hint, reason) = match error_code {
        Some(30001) => (
            TwilioSmsFailureClass::Retryable,
            Some(Duration::from_secs(60)),
            "twilio sms queue overflow; retry later or use a Messaging Service with more sender throughput",
        ),
        Some(30003) => (
            TwilioSmsFailureClass::Retryable,
            Some(Duration::from_secs(300)),
            "destination handset was temporarily unreachable",
        ),
        Some(30005) => (
            TwilioSmsFailureClass::Retryable,
            Some(Duration::from_secs(300)),
            "destination handset or carrier path was unavailable",
        ),
        Some(30008) => (
            TwilioSmsFailureClass::Retryable,
            Some(Duration::from_secs(300)),
            "carrier returned an unknown delivery failure that may clear later",
        ),
        Some(30034) => (
            TwilioSmsFailureClass::Permanent,
            None,
            "sender is not associated with an approved US A2P 10DLC campaign",
        ),
        Some(21610) => (
            TwilioSmsFailureClass::Permanent,
            None,
            "recipient has opted out from this Twilio sender or Messaging Service",
        ),
        Some(30007) => (
            TwilioSmsFailureClass::Permanent,
            None,
            "message was filtered by Twilio or the carrier",
        ),
        _ if status == "canceled" => (
            TwilioSmsFailureClass::Permanent,
            None,
            "message was canceled before delivery",
        ),
        _ => (
            TwilioSmsFailureClass::Permanent,
            None,
            "twilio sms reached a terminal failed delivery state",
        ),
    };

    Some(TwilioSmsDeliveryFailure {
        status: status.to_string(),
        error_code,
        error_message: error_message.map(ToOwned::to_owned),
        class,
        backoff_hint,
        reason: reason.to_string(),
    })
}

impl TwilioSmsMessage {
    fn to_request(
        &self,
        default_from: Option<&str>,
        default_messaging_service_sid: Option<&str>,
    ) -> Result<TwilioSmsRequest> {
        let to = required_field("to", &self.to)?;
        let body = required_field("body", &self.body)?;
        if body.chars().count() > 1600 {
            return Err(MoaError::ValidationError(
                "twilio sms body cannot exceed 1600 characters".to_string(),
            ));
        }

        let from = self
            .from
            .as_deref()
            .and_then(optional_field)
            .or_else(|| default_from.and_then(optional_field));
        let messaging_service_sid = self
            .messaging_service_sid
            .as_deref()
            .and_then(optional_field)
            .or_else(|| default_messaging_service_sid.and_then(optional_field));
        if from.is_none() && messaging_service_sid.is_none() {
            return Err(MoaError::ValidationError(
                "twilio sms requires from or messaging_service_sid".to_string(),
            ));
        }

        Ok(TwilioSmsRequest {
            to,
            body,
            from,
            messaging_service_sid,
            status_callback: self.status_callback.as_deref().and_then(optional_field),
        })
    }
}

#[derive(Debug)]
struct TwilioSmsRequest {
    to: String,
    body: String,
    from: Option<String>,
    messaging_service_sid: Option<String>,
    status_callback: Option<String>,
}

impl TwilioSmsRequest {
    fn to_form(&self) -> Vec<(&'static str, String)> {
        let mut form = vec![("To", self.to.clone()), ("Body", self.body.clone())];
        if let Some(from) = &self.from {
            form.push(("From", from.clone()));
        }
        if let Some(messaging_service_sid) = &self.messaging_service_sid {
            form.push(("MessagingServiceSid", messaging_service_sid.clone()));
        }
        if let Some(status_callback) = &self.status_callback {
            form.push(("StatusCallback", status_callback.clone()));
        }
        form
    }
}

#[derive(Debug, Deserialize)]
struct TwilioSmsResponse {
    sid: String,
    status: String,
    to: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    messaging_service_sid: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    error_message: Option<String>,
    uri: String,
}

#[derive(Debug, Deserialize)]
struct TwilioApiErrorResponse {
    #[serde(default)]
    code: Option<i64>,
}

impl From<TwilioSmsResponse> for TwilioSmsSendResult {
    fn from(value: TwilioSmsResponse) -> Self {
        Self {
            sid: value.sid,
            status: value.status,
            to: value.to,
            from: value.from,
            messaging_service_sid: value.messaging_service_sid,
            error_code: value.error_code,
            error_message: value.error_message,
            uri: value.uri,
        }
    }
}

async fn required_vault_string(
    vault: &Arc<dyn CredentialVault>,
    service: &str,
    scope: &str,
) -> Result<String> {
    let credential = vault.get(service, scope).await?;
    twilio_string_from_credential(service, credential)
}

async fn optional_vault_string(
    vault: &Arc<dyn CredentialVault>,
    service: &str,
    scope: &str,
) -> Result<Option<String>> {
    match vault.get(service, scope).await {
        Ok(credential) => twilio_string_from_credential(service, credential).map(Some),
        Err(MoaError::MissingEnvironmentVariable(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn twilio_string_from_credential(service: &str, credential: Credential) -> Result<String> {
    match credential {
        Credential::Bearer(value) => Ok(value),
        Credential::ApiKey { value, .. } => Ok(value),
        Credential::OAuth { .. } => Err(MoaError::ConfigError(format!(
            "{service} credential must be bearer or api_key"
        ))),
    }
}

fn required_field(name: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(MoaError::ValidationError(format!(
            "twilio sms {name} is required"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sms_request_requires_sender_or_messaging_service() {
        // Pins: Twilio requires a sender value through From or MessagingServiceSid.
        let message = TwilioSmsMessage::new("+15005550006", "body");
        let error = message
            .to_request(None, None)
            .expect_err("missing Twilio sender should be rejected before HTTP send");
        assert!(matches!(error, MoaError::ValidationError(_)));
    }

    #[test]
    fn sms_request_applies_default_sender_and_rejects_empty_body() {
        // Pins: default sender configuration is applied while empty content is rejected.
        let message = TwilioSmsMessage::new("+15005550006", "body");
        let request = message
            .to_request(Some("+15551234567"), None)
            .expect("valid Twilio SMS should build a request");
        assert_eq!(request.from.as_deref(), Some("+15551234567"));
        assert_eq!(request.body, "body");

        let error = TwilioSmsMessage::new("+15005550006", " ")
            .with_from("+15551234567")
            .to_request(None, None)
            .expect_err("empty SMS body should be rejected before HTTP send");
        assert!(matches!(error, MoaError::ValidationError(_)));
    }
}
