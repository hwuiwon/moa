//! Typed Slack Web API error classification and telemetry.

use std::time::Duration;

use moa_core::error::MoaError;
use slack_morphism::errors::SlackClientError;
use tracing::field;

pub(super) const SLACK_RATE_LIMIT_RETRIES: usize = 3;

/// Retry classification for a Slack Web API failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackApiFailureClass {
    /// A later API call may succeed after the referenced transient condition clears.
    Retryable,
    /// A retry is not expected to help without configuration, permission, or request changes.
    Permanent,
}

impl SlackApiFailureClass {
    /// Returns the stable telemetry label for this failure class.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::Permanent => "permanent",
        }
    }
}

/// Structured Slack Web API failure metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackApiFailure {
    /// Slack Web API error code when the response provided one.
    pub code: Option<String>,
    /// HTTP status returned by Slack when available.
    pub http_status: Option<u16>,
    /// Parsed `Retry-After` hint when Slack provided one.
    pub retry_after: Option<Duration>,
    /// Retry classification for this failure.
    pub class: SlackApiFailureClass,
    /// Human-readable reason safe for logs and operator UI.
    pub reason: String,
}

impl SlackApiFailure {
    /// Returns whether this Slack API failure can be retried by a durable caller.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.class == SlackApiFailureClass::Retryable
    }
}

pub(super) fn slack_client_error(operation: &'static str, error: SlackClientError) -> MoaError {
    let failure = classify_slack_client_error(&error);
    record_slack_failure(operation, &failure);
    match error {
        SlackClientError::RateLimitError(rate_limit) => MoaError::RateLimited {
            retries: SLACK_RATE_LIMIT_RETRIES,
            message: slack_failure_message(&failure, rate_limit.http_response_body.as_deref()),
        },
        SlackClientError::HttpError(http) => {
            let status = http.status_code.as_u16();
            if status == 429 {
                return MoaError::RateLimited {
                    retries: SLACK_RATE_LIMIT_RETRIES,
                    message: slack_failure_message(&failure, http.http_response_body.as_deref()),
                };
            }
            MoaError::HttpStatus {
                status,
                retry_after: failure.retry_after,
                message: http
                    .http_response_body
                    .unwrap_or_else(|| failure.reason.clone()),
            }
        }
        _ if failure.is_retryable() => MoaError::ProviderQuirk(failure.reason),
        _ => MoaError::ProviderError(failure.reason),
    }
}

pub(super) fn classify_slack_client_error(error: &SlackClientError) -> SlackApiFailure {
    match error {
        SlackClientError::RateLimitError(rate_limit) => SlackApiFailure {
            code: rate_limit.code.clone(),
            http_status: Some(429),
            retry_after: rate_limit.retry_after,
            class: SlackApiFailureClass::Retryable,
            reason: "slack Web API rate limit was exceeded".to_string(),
        },
        SlackClientError::HttpError(http) => {
            let status = http.status_code.as_u16();
            let class = if is_retryable_slack_http_status(status) {
                SlackApiFailureClass::Retryable
            } else {
                SlackApiFailureClass::Permanent
            };
            SlackApiFailure {
                code: None,
                http_status: Some(status),
                retry_after: None,
                class,
                reason: format!("slack Web API returned HTTP status {status}"),
            }
        }
        SlackClientError::ApiError(api) => {
            let class = classify_slack_api_code(&api.code);
            SlackApiFailure {
                code: Some(api.code.clone()),
                http_status: None,
                retry_after: None,
                class,
                reason: format!("slack Web API returned error code {}", api.code),
            }
        }
        SlackClientError::HttpProtocolError(_) | SlackClientError::SystemError(_) => {
            SlackApiFailure {
                code: None,
                http_status: None,
                retry_after: Some(Duration::from_secs(1)),
                class: SlackApiFailureClass::Retryable,
                reason: error.to_string(),
            }
        }
        SlackClientError::EndOfStream(_)
        | SlackClientError::ProtocolError(_)
        | SlackClientError::SocketModeProtocolError(_) => SlackApiFailure {
            code: None,
            http_status: None,
            retry_after: None,
            class: SlackApiFailureClass::Permanent,
            reason: error.to_string(),
        },
    }
}

fn classify_slack_api_code(code: &str) -> SlackApiFailureClass {
    match code {
        "fatal_error"
        | "internal_error"
        | "request_timeout"
        | "service_unavailable"
        | "ratelimited" => SlackApiFailureClass::Retryable,
        _ => SlackApiFailureClass::Permanent,
    }
}

fn is_retryable_slack_http_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn slack_failure_message(failure: &SlackApiFailure, body: Option<&str>) -> String {
    match body.filter(|body| !body.trim().is_empty()) {
        Some(body) => format!("{}: {body}", failure.reason),
        None => failure.reason.clone(),
    }
}

pub(super) fn slack_api_span(name: &'static str, method: &'static str) -> tracing::Span {
    tracing::info_span!(
        "slack_api",
        otel.name = name,
        messaging.system = "slack",
        messaging.operation = method,
        messaging.channel = "slack",
        slack.method = method,
        slack.error_code = field::Empty,
        slack.failure_class = field::Empty,
        slack.retryable = field::Empty,
        slack.retry_after_ms = field::Empty,
        http.status_code = field::Empty,
        error = field::Empty,
    )
}

fn record_slack_failure(operation: &'static str, failure: &SlackApiFailure) {
    let span = tracing::Span::current();
    if let Some(status) = failure.http_status {
        span.record("http.status_code", status);
    }
    if let Some(code) = failure.code.as_deref() {
        span.record("slack.error_code", code);
    }
    if let Some(retry_after) = failure.retry_after {
        span.record("slack.retry_after_ms", retry_after.as_millis() as u64);
    }
    span.record("slack.failure_class", failure.class.label());
    span.record("slack.retryable", failure.is_retryable());
    span.record("error", failure.reason.as_str());

    if failure.is_retryable() {
        tracing::warn!(
            messaging.system = "slack",
            messaging.operation = operation,
            slack.error_code = ?failure.code,
            http.status_code = ?failure.http_status,
            slack.failure_class = failure.class.label(),
            slack.retryable = true,
            error = %failure.reason,
            "slack Web API returned a retryable failure"
        );
    } else {
        tracing::error!(
            messaging.system = "slack",
            messaging.operation = operation,
            slack.error_code = ?failure.code,
            http.status_code = ?failure.http_status,
            slack.failure_class = failure.class.label(),
            slack.retryable = false,
            error = %failure.reason,
            "slack Web API returned a permanent failure"
        );
    }
}
