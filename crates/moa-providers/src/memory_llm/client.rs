//! Shared Cohere chat transport for memory ingestion LLM calls.

use std::time::Duration;

use rand::Rng;
use reqwest::{Client, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::core::pacer::{PacerConfig, RatePacer};

const COHERE_CHAT_URL: &str = "https://api.cohere.com/v2/chat";
const RETRY_DELAY: Duration = Duration::from_millis(200);
/// Documented Cohere Chat production limit: 500 requests/min (trial keys: 20).
const COHERE_CHAT_REQUESTS_PER_MIN: u32 = 500;

/// Cohere chat client shared by ingestion components that need text generation.
#[derive(Clone)]
pub struct LlmChatClient {
    client: Client,
    endpoint: String,
    model: String,
    api_key: SecretString,
    pacer: RatePacer,
}

impl LlmChatClient {
    /// Creates a Cohere chat client from an explicit API key.
    ///
    /// Requests are paced to the documented Cohere Chat production ceiling
    /// (500 req/min) by default; a trial key should lower this with
    /// [`LlmChatClient::with_rate_limits`] (e.g.
    /// `PacerConfig::requests_per_min(20)`).
    #[must_use]
    pub fn from_api_key(api_key: SecretString, model: &str, timeout_ms: u64) -> Self {
        let timeout = Duration::from_millis(timeout_ms);
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            endpoint: COHERE_CHAT_URL.to_string(),
            model: model.to_string(),
            api_key,
            pacer: RatePacer::new(PacerConfig::requests_per_min(COHERE_CHAT_REQUESTS_PER_MIN)),
        }
    }

    /// Overrides the HTTP client, primarily for deterministic tests.
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    /// Overrides the per-minute request pacing, e.g. to apply a trial key's
    /// lower Cohere Chat ceiling (`PacerConfig::requests_per_min(20)`).
    #[must_use]
    pub fn with_rate_limits(mut self, config: PacerConfig) -> Self {
        self.pacer = RatePacer::new(config);
        self
    }

    /// Overrides the Cohere chat endpoint, primarily for deterministic tests.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Sends a non-streaming chat request and returns the assistant text.
    pub async fn chat(&self, system: &str, user: &str) -> Result<String, LlmChatError> {
        self.chat_with_retry(system, user).await
    }

    async fn chat_with_retry(&self, system: &str, user: &str) -> Result<String, LlmChatError> {
        let mut attempt = 0_u8;
        loop {
            match self.chat_once(system, user).await {
                Ok(text) => return Ok(text),
                Err(error) if error.is_retryable() && attempt == 0 => {
                    attempt += 1;
                    sleep(retry_delay_with_jitter()).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn chat_once(
        &self,
        system: &str,
        user: &str,
    ) -> std::result::Result<String, LlmChatError> {
        let mut messages = Vec::with_capacity(2);
        if !system.trim().is_empty() {
            messages.push(CohereChatMessage {
                role: "system",
                content: system,
            });
        }
        messages.push(CohereChatMessage {
            role: "user",
            content: user,
        });
        let request = CohereChatRequest {
            stream: false,
            model: &self.model,
            messages,
            temperature: 0.0,
        };
        // Cohere Chat is limited by requests/min; pace before dispatching.
        self.pacer.acquire(1, 0).await;
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.api_key.expose_secret())
            .json(&request)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("failed to read error body: {error}"));
            return Err(error_for_status(status, body));
        }
        let body = response
            .json::<CohereChatResponse>()
            .await
            .map_err(|error| LlmChatError::Malformed {
                message: format!("failed to parse Cohere chat response: {error}"),
            })?;
        let text = body
            .message
            .content
            .into_iter()
            .filter_map(|part| match part {
                CohereChatContent::Text { text } => Some(text),
                CohereChatContent::Other => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if text.trim().is_empty() {
            return Err(LlmChatError::Malformed {
                message: "Cohere chat response did not contain assistant text".to_string(),
            });
        }
        Ok(text)
    }
}

/// Typed chat transport failure categories.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum LlmChatError {
    /// Authentication or credential lookup failed.
    #[error("llm chat auth: {message}")]
    Auth {
        /// Human-readable failure details without secret values.
        message: String,
    },
    /// The HTTP request timed out.
    #[error("llm chat timeout: {message}")]
    Timeout {
        /// Human-readable timeout details.
        message: String,
    },
    /// A retryable transport or provider error occurred.
    #[error("llm chat transient: {message}")]
    Transient {
        /// Human-readable transient failure details.
        message: String,
    },
    /// The provider returned an unexpected response shape or non-retryable status.
    #[error("llm chat malformed: {message}")]
    Malformed {
        /// Human-readable malformed-response details.
        message: String,
    },
}

impl LlmChatError {
    fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout { .. } | Self::Transient { .. })
    }
}

/// Returns the retry backoff with proportional jitter applied.
///
/// Adds a uniform random delay of up to half the base [`RETRY_DELAY`] so
/// concurrent ingestion retries do not resynchronize into a thundering herd
/// against the shared Cohere chat endpoint.
fn retry_delay_with_jitter() -> Duration {
    let base_ms = RETRY_DELAY.as_millis() as u64;
    let jitter_ms = rand::thread_rng().gen_range(0..=base_ms / 2);
    RETRY_DELAY + Duration::from_millis(jitter_ms)
}

fn map_reqwest_error(error: reqwest::Error) -> LlmChatError {
    if error.is_timeout() {
        LlmChatError::Timeout {
            message: error.to_string(),
        }
    } else if error.is_connect() || error.is_request() {
        LlmChatError::Transient {
            message: error.to_string(),
        }
    } else {
        LlmChatError::Malformed {
            message: error.to_string(),
        }
    }
}

fn error_for_status(status: StatusCode, body: String) -> LlmChatError {
    match status.as_u16() {
        401 | 403 => LlmChatError::Auth {
            message: format!("Cohere chat returned HTTP {}: {body}", status.as_u16()),
        },
        429 => LlmChatError::Transient {
            message: format!("Cohere chat returned HTTP 429: {body}"),
        },
        _ if status.is_server_error() => LlmChatError::Transient {
            message: format!("Cohere chat returned HTTP {}: {body}", status.as_u16()),
        },
        _ => LlmChatError::Malformed {
            message: format!("Cohere chat returned HTTP {}: {body}", status.as_u16()),
        },
    }
}

#[derive(Serialize)]
struct CohereChatRequest<'a> {
    stream: bool,
    model: &'a str,
    messages: Vec<CohereChatMessage<'a>>,
    temperature: f64,
}

#[derive(Serialize)]
struct CohereChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct CohereChatResponse {
    message: CohereAssistantMessage,
}

#[derive(Deserialize)]
struct CohereAssistantMessage {
    content: Vec<CohereChatContent>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CohereChatContent {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use secrecy::SecretString;
    use tokio::net::TcpListener;
    use tokio::time::sleep;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn llm_client_maps_timeouts_and_http_errors_to_typed_errors() {
        // Pins: shared Cohere transport classifies auth, timeout, transient, and malformed errors.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/malformed"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/retry"))
            .respond_with(ResponseTemplate::new(500).set_body_string("try later"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/retry"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": [{"type": "text", "text": "ok"}]}
            })))
            .mount(&server)
            .await;

        let auth = client(500).with_endpoint(format!("{}/auth", server.uri()));
        let error = auth.chat("", "hi").await.expect_err("auth should fail");
        assert!(matches!(error, LlmChatError::Auth { .. }));

        let malformed = client(500).with_endpoint(format!("{}/malformed", server.uri()));
        let error = malformed
            .chat("", "hi")
            .await
            .expect_err("malformed status should fail");
        assert!(matches!(error, LlmChatError::Malformed { .. }));

        let retry = client(500).with_endpoint(format!("{}/retry", server.uri()));
        assert_eq!(retry.chat("", "hi").await.expect("retry succeeds"), "ok");

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind timeout listener");
        let addr = listener.local_addr().expect("listener addr");
        tokio::spawn(async move {
            if let Ok((_socket, _peer)) = listener.accept().await {
                sleep(Duration::from_secs(1)).await;
            }
        });
        let timeout = client(10).with_endpoint(format!("http://{addr}/timeout"));
        let error = timeout
            .chat("", "hi")
            .await
            .expect_err("timeout should fail");
        assert!(matches!(error, LlmChatError::Timeout { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn chat_client_defaults_to_cohere_chat_request_pacing() {
        // Pins: `from_api_key` wires a requests/min pacer at the documented Cohere
        // Chat ceiling, so once that per-minute budget is spent the next request
        // waits for the bucket to refill. Exercises the exact pacer the chat path
        // acquires; pacing is not observable through the HTTP seam under paused
        // time because auto-advanced virtual time trips the request timeout.
        let client = client(5_000);
        client
            .pacer
            .acquire(super::COHERE_CHAT_REQUESTS_PER_MIN, 0)
            .await;

        let before = tokio::time::Instant::now();
        client.pacer.acquire(1, 0).await;
        // One token refills at limit/60 per second, so the wait is > 0 once drained.
        assert!(
            before.elapsed() >= Duration::from_millis(100),
            "a drained default budget should pace the next request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn chat_client_rate_limit_override_lowers_the_ceiling() {
        // Pins: `with_rate_limits` replaces the default pacer, e.g. a trial key's
        // 1/min ceiling, so the second request waits ~60s for one token to refill.
        let client = client(5_000).with_rate_limits(PacerConfig::requests_per_min(1));
        client.pacer.acquire(1, 0).await;

        let before = tokio::time::Instant::now();
        client.pacer.acquire(1, 0).await;
        assert!(
            before.elapsed() >= Duration::from_secs(55),
            "a 1/min override should pace the next request by ~60s"
        );
    }

    fn client(timeout_ms: u64) -> LlmChatClient {
        LlmChatClient::from_api_key(SecretString::from("test-key"), "command-test", timeout_ms)
    }
}
