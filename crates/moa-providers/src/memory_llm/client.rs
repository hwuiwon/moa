//! Shared Cohere chat transport for memory ingestion LLM calls.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

const COHERE_CHAT_URL: &str = "https://api.cohere.com/v2/chat";
const RETRY_DELAY: Duration = Duration::from_millis(200);

/// Cohere chat client shared by ingestion components that need text generation.
#[derive(Clone)]
pub struct LlmChatClient {
    client: Client,
    endpoint: String,
    model: String,
    api_key: SecretString,
}

impl LlmChatClient {
    /// Creates a Cohere chat client from an explicit API key.
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
        }
    }

    /// Overrides the HTTP client, primarily for deterministic tests.
    #[must_use]
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
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
        let mut attempt = 0_u8;
        loop {
            match self.chat_once(system, user).await {
                Ok(text) => return Ok(text),
                Err(error) if error.is_retryable() && attempt == 0 => {
                    attempt += 1;
                    sleep(RETRY_DELAY).await;
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

    fn client(timeout_ms: u64) -> LlmChatClient {
        LlmChatClient::from_api_key(SecretString::from("test-key"), "command-test", timeout_ms)
    }
}
