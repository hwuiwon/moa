//! Slack platform adapter built on top of `slack-morphism` Socket Mode.

use std::{collections::HashMap, env, sync::Arc, time::Duration, time::Instant};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    ChannelRef, InboundMessage, MessageId, MoaConfig, MoaError, OutboundMessage, Platform,
    PlatformAdapter, PlatformCapabilities, PlatformUser, Result,
};
use slack_morphism::errors::SlackClientError;
use slack_morphism::prelude::*;
use tokio::{
    sync::{RwLock, mpsc},
    time::sleep,
};
use tracing::{Instrument, field, warn};
use uuid::Uuid;

use crate::{
    action_review::prepare_outbound_message,
    messaging_receive_span,
    renderer::{SlackRenderChunk, SlackRenderer},
};

const SLACK_RATE_LIMIT_RETRIES: usize = 3;

#[derive(Clone)]
struct SlackListenerState {
    event_tx: mpsc::Sender<InboundMessage>,
    inbound_contexts: Arc<RwLock<HashMap<String, SlackMessageRef>>>,
}

/// Slack adapter implementing the generic platform abstraction.
#[derive(Clone)]
pub struct SlackAdapter {
    client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    bot_token: SlackApiToken,
    app_token: SlackApiToken,
    renderer: SlackRenderer,
    inbound_contexts: Arc<RwLock<HashMap<String, SlackMessageRef>>>,
    outbound_messages: Arc<RwLock<HashMap<String, Vec<SlackMessageRef>>>>,
    last_edits: Arc<RwLock<HashMap<String, Instant>>>,
}

impl SlackAdapter {
    /// Creates a Slack adapter from a bot token and an app-level Socket Mode token.
    pub fn new(bot_token: impl Into<String>, app_token: impl Into<String>) -> Result<Self> {
        let connector = SlackClientHyperConnector::new()
            .map_err(|error| MoaError::ProviderError(error.to_string()))?
            .with_rate_control(
                SlackApiRateControlConfig::new().with_max_retries(SLACK_RATE_LIMIT_RETRIES),
            );
        let client = Arc::new(SlackClient::new(connector));
        Ok(Self {
            client,
            bot_token: SlackApiToken {
                token_value: SlackApiTokenValue(bot_token.into()),
                cookie: None,
                team_id: None,
                scope: None,
                token_type: Some(SlackApiTokenType::Bot),
            },
            app_token: SlackApiToken {
                token_value: SlackApiTokenValue(app_token.into()),
                cookie: None,
                team_id: None,
                scope: None,
                token_type: Some(SlackApiTokenType::App),
            },
            renderer: SlackRenderer::new(),
            inbound_contexts: Arc::new(RwLock::new(HashMap::new())),
            outbound_messages: Arc::new(RwLock::new(HashMap::new())),
            last_edits: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Creates a Slack adapter using the configured token environment variables.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let bot_env = &config.messaging.slack_token_env;
        let app_env = &config.messaging.slack_app_token_env;
        let bot_token =
            env::var(bot_env).map_err(|_| MoaError::MissingEnvironmentVariable(bot_env.clone()))?;
        let app_token =
            env::var(app_env).map_err(|_| MoaError::MissingEnvironmentVariable(app_env.clone()))?;
        Self::new(bot_token, app_token)
    }

    async fn resolve_target(&self, reply_to: Option<&str>) -> Result<SlackTarget> {
        let reply_to = reply_to.ok_or_else(|| {
            MoaError::ValidationError("slack outbound messages require reply_to context".into())
        })?;

        if let Some(last_ref) = self
            .outbound_messages
            .read()
            .await
            .get(reply_to)
            .and_then(|refs| refs.last().cloned())
        {
            return Ok(last_ref.target());
        }

        if let Some(inbound_ref) = self.inbound_contexts.read().await.get(reply_to).cloned() {
            return Ok(inbound_ref.target());
        }

        Err(MoaError::ValidationError(format!(
            "slack reply target not found: {reply_to}"
        )))
    }

    async fn send_chunk(
        &self,
        target: &SlackTarget,
        chunk: &SlackRenderChunk,
    ) -> Result<SlackMessageRef> {
        let session = self.client.open_session(&self.bot_token);
        let request = SlackApiChatPostMessageRequest {
            channel: SlackChannelId(target.channel_id.to_string()),
            content: slack_message_content(chunk),
            as_user: None,
            icon_emoji: None,
            icon_url: None,
            link_names: None,
            parse: None,
            thread_ts: Some(SlackTs(target.thread_ts.clone())),
            username: None,
            reply_broadcast: None,
            unfurl_links: None,
            unfurl_media: None,
        };

        async {
            let response = session
                .chat_post_message(&request)
                .await
                .map_err(|error| slack_client_error("chat.postMessage", error))?;
            Ok(SlackMessageRef {
                channel_id: Arc::<str>::from(response.channel.0),
                ts: response.ts.0,
                thread_ts: Some(target.thread_ts.clone()),
            })
        }
        .instrument(slack_api_span("slack_message_send", "chat.postMessage"))
        .await
    }

    async fn update_chunk(
        &self,
        message_ref: &SlackMessageRef,
        chunk: &SlackRenderChunk,
    ) -> Result<()> {
        let session = self.client.open_session(&self.bot_token);
        let request = SlackApiChatUpdateRequest {
            channel: SlackChannelId(message_ref.channel_id.to_string()),
            content: slack_message_content(chunk),
            ts: SlackTs(message_ref.ts.clone()),
            as_user: None,
            link_names: None,
            parse: None,
            reply_broadcast: None,
        };

        async {
            session
                .chat_update(&request)
                .await
                .map_err(|error| slack_client_error("chat.update", error))?;
            Ok(())
        }
        .instrument(slack_api_span("slack_message_update", "chat.update"))
        .await
    }

    async fn wait_for_edit_window(&self, message_id: &MessageId) {
        let min_interval = self.capabilities().min_edit_interval;
        let sleep_for = {
            let last_edits = self.last_edits.read().await;
            last_edits
                .get(message_id.as_str())
                .copied()
                .map(|last_edit| min_interval.saturating_sub(last_edit.elapsed()))
        };
        if let Some(delay) = sleep_for.filter(|delay| !delay.is_zero()) {
            sleep(delay).await;
        }
        self.last_edits
            .write()
            .await
            .insert(message_id.as_str().to_string(), Instant::now());
    }
}

/// Normalizes one Slack Events API callback JSON payload into MOA's canonical inbound shape.
pub fn normalize_event_json(payload: &str) -> Result<InboundMessage> {
    let event: SlackPushEventCallback = serde_json::from_str(payload)?;
    normalize_push_event(&event)
}

/// Normalizes one parsed Slack push event into MOA's canonical inbound shape.
pub fn normalize_push_event(event: &SlackPushEventCallback) -> Result<InboundMessage> {
    inbound_from_push_event(event).ok_or_else(|| {
        MoaError::ValidationError("slack event is not a supported user message".to_string())
    })
}

#[async_trait]
impl PlatformAdapter for SlackAdapter {
    /// Returns the adapter platform identifier.
    fn platform(&self) -> Platform {
        self.renderer.platform()
    }

    /// Returns Slack transport capabilities.
    fn capabilities(&self) -> PlatformCapabilities {
        self.renderer.capabilities()
    }

    /// Starts the Slack Socket Mode listener and forwards normalized updates.
    async fn start(&self, event_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let client = self.client.clone();
        let callbacks = SlackSocketModeListenerCallbacks::new()
            .with_push_events(handle_push_event)
            .with_interaction_events(handle_interaction_event);

        let listener_environment = Arc::new(
            SlackClientEventsListenerEnvironment::new(client).with_user_state(SlackListenerState {
                event_tx,
                inbound_contexts: self.inbound_contexts.clone(),
            }),
        );
        let listener = SlackClientSocketModeListener::new(
            &SlackClientSocketModeConfig {
                max_connections_count: SlackClientSocketModeConfig::DEFAULT_CONNECTIONS_COUNT,
                debug_connections: SlackClientSocketModeConfig::DEFAULT_DEBUG_CONNECTIONS,
                initial_backoff_in_seconds:
                    SlackClientSocketModeConfig::DEFAULT_INITIAL_BACKOFF_IN_SECONDS,
                reconnect_timeout_in_seconds:
                    SlackClientSocketModeConfig::DEFAULT_RECONNECT_TIMEOUT_IN_SECONDS,
                ping_interval_in_seconds:
                    SlackClientSocketModeConfig::DEFAULT_PING_INTERVAL_IN_SECONDS,
                ping_failure_threshold_times:
                    SlackClientSocketModeConfig::DEFAULT_PING_FAILURE_THRESHOLD_TIMES,
            },
            listener_environment,
            callbacks,
        );
        listener
            .listen_for(&self.app_token)
            .await
            .map_err(|error| slack_client_error("socket_mode.listen_for", error))?;
        listener.serve().await;
        Ok(())
    }

    /// Sends a new outbound Slack message, splitting at Slack's length limit.
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        let msg = prepare_outbound_message(self.platform(), &self.capabilities(), msg);
        let target = self.resolve_target(msg.reply_to.as_deref()).await?;
        let rendered = self.renderer.render(&msg);
        let synthetic_id = MessageId::new(Uuid::now_v7().to_string());
        let mut sent_refs = Vec::with_capacity(rendered.len());
        for chunk in &rendered {
            let sent_ref = self.send_chunk(&target, chunk).await?;
            sent_refs.push(sent_ref);
        }
        self.outbound_messages
            .write()
            .await
            .insert(synthetic_id.as_str().to_string(), sent_refs);
        Ok(synthetic_id)
    }

    /// Edits an existing outbound Slack message in place.
    async fn edit(&self, msg_id: &MessageId, msg: OutboundMessage) -> Result<()> {
        self.wait_for_edit_window(msg_id).await;
        let msg = prepare_outbound_message(self.platform(), &self.capabilities(), msg);

        let existing = self
            .outbound_messages
            .read()
            .await
            .get(msg_id.as_str())
            .cloned()
            .ok_or_else(|| {
                MoaError::ValidationError(format!("unknown slack message id: {msg_id}"))
            })?;
        let rendered = self.renderer.render(&msg);
        let overlap = existing.len().min(rendered.len());
        let mut updated_refs = Vec::with_capacity(rendered.len());

        for index in 0..overlap {
            let message_ref = existing[index].clone();
            self.update_chunk(&message_ref, &rendered[index]).await?;
            updated_refs.push(message_ref);
        }

        if rendered.len() > existing.len() {
            let target = existing
                .last()
                .cloned()
                .map(|message_ref| message_ref.target())
                .ok_or_else(|| {
                    MoaError::ValidationError(format!("slack message id {msg_id} has no refs"))
                })?;
            for chunk in rendered.iter().skip(existing.len()) {
                let sent_ref = self.send_chunk(&target, chunk).await?;
                updated_refs.push(sent_ref);
            }
        }

        if existing.len() > rendered.len() {
            let session = self.client.open_session(&self.bot_token);
            for message_ref in existing.iter().skip(rendered.len()) {
                let request = SlackApiChatDeleteRequest {
                    channel: SlackChannelId(message_ref.channel_id.to_string()),
                    ts: SlackTs(message_ref.ts.clone()),
                    as_user: None,
                };
                session
                    .chat_delete(&request)
                    .await
                    .map_err(|error| slack_client_error("chat.delete", error))?;
            }
        }

        self.outbound_messages
            .write()
            .await
            .insert(msg_id.as_str().to_string(), updated_refs);
        Ok(())
    }

    /// Deletes a Slack message sent through this adapter.
    async fn delete(&self, msg_id: &MessageId) -> Result<()> {
        let refs = self
            .outbound_messages
            .write()
            .await
            .remove(msg_id.as_str())
            .ok_or_else(|| {
                MoaError::ValidationError(format!("unknown slack message id: {msg_id}"))
            })?;
        let session = self.client.open_session(&self.bot_token);
        for message_ref in refs {
            let request = SlackApiChatDeleteRequest {
                channel: SlackChannelId(message_ref.channel_id.to_string()),
                ts: SlackTs(message_ref.ts),
                as_user: None,
            };
            session
                .chat_delete(&request)
                .await
                .map_err(|error| slack_client_error("chat.delete", error))?;
        }
        Ok(())
    }
}

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

fn slack_client_error(operation: &'static str, error: SlackClientError) -> MoaError {
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

fn classify_slack_client_error(error: &SlackClientError) -> SlackApiFailure {
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

fn slack_api_span(name: &'static str, method: &'static str) -> tracing::Span {
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

async fn handle_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    state: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    let shared = {
        let guard = state.read().await;
        guard.get_user_state::<SlackListenerState>().cloned()
    };
    let Some(shared) = shared else {
        warn!("slack listener state missing for push event");
        return Ok(());
    };

    if let Some(inbound) = inbound_from_push_event(&event) {
        let messaging_span = messaging_receive_span(&inbound);
        async {
            if let Some(origin) = push_event_origin(&event) {
                shared
                    .inbound_contexts
                    .write()
                    .await
                    .insert(inbound.platform_msg_id.clone(), origin);
            }
            if shared.event_tx.send(inbound).await.is_err() {
                warn!("slack inbound receiver dropped");
            }
        }
        .instrument(messaging_span)
        .await;
    }
    Ok(())
}

async fn handle_interaction_event(
    event: SlackInteractionEvent,
    _client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    state: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    let shared = {
        let guard = state.read().await;
        guard.get_user_state::<SlackListenerState>().cloned()
    };
    let Some(shared) = shared else {
        warn!("slack listener state missing for interaction event");
        return Ok(());
    };

    if let Some(inbound) = inbound_from_interaction_event(&event, shared.inbound_contexts).await {
        let messaging_span = messaging_receive_span(&inbound);
        async {
            if shared.event_tx.send(inbound).await.is_err() {
                warn!("slack inbound receiver dropped");
            }
        }
        .instrument(messaging_span)
        .await;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlackTarget {
    channel_id: Arc<str>,
    thread_ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlackMessageRef {
    channel_id: Arc<str>,
    ts: String,
    thread_ts: Option<String>,
}

impl SlackMessageRef {
    fn target(&self) -> SlackTarget {
        SlackTarget {
            channel_id: self.channel_id.clone(),
            thread_ts: self.thread_anchor().to_string(),
        }
    }

    fn thread_anchor(&self) -> &str {
        self.thread_ts.as_deref().unwrap_or(self.ts.as_str())
    }
}

fn inbound_from_push_event(event: &SlackPushEventCallback) -> Option<InboundMessage> {
    match &event.event {
        SlackEventCallbackBody::AppMention(message) => inbound_from_app_mention(message),
        SlackEventCallbackBody::Message(message) => inbound_from_message_event(message),
        _ => None,
    }
}

fn push_event_origin(event: &SlackPushEventCallback) -> Option<SlackMessageRef> {
    match &event.event {
        SlackEventCallbackBody::AppMention(message) => Some(SlackMessageRef {
            channel_id: Arc::<str>::from(message.channel.0.clone()),
            ts: message.origin.ts.0.clone(),
            thread_ts: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        }),
        SlackEventCallbackBody::Message(message) => Some(SlackMessageRef {
            channel_id: Arc::<str>::from(message.origin.channel.as_ref()?.0.clone()),
            ts: message.origin.ts.0.clone(),
            thread_ts: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        }),
        _ => None,
    }
}

async fn inbound_from_interaction_event(
    event: &SlackInteractionEvent,
    _inbound_contexts: Arc<RwLock<HashMap<String, SlackMessageRef>>>,
) -> Option<InboundMessage> {
    let SlackInteractionEvent::BlockActions(_) = event else {
        return None;
    };
    None
}

fn inbound_from_app_mention(message: &SlackAppMentionEvent) -> Option<InboundMessage> {
    let text = message.content.text.clone()?;
    let platform_msg_id = message.origin.ts.0.clone();
    let user_id = message.user.0.clone();
    Some(InboundMessage {
        platform: Platform::Slack,
        platform_msg_id,
        user: PlatformUser {
            platform_id: user_id.clone(),
            display_name: format!("<@{}>", message.user.0),
            moa_user_id: None,
        },
        channel: slack_channel_ref(
            &message.channel.0,
            message.origin.thread_ts.as_ref().map(|ts| ts.0.as_str()),
            &user_id,
        ),
        text,
        attachments: Vec::new(),
        reply_to: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        timestamp: slack_ts_to_datetime(&message.origin.ts.0),
    })
}

fn inbound_from_message_event(message: &SlackMessageEvent) -> Option<InboundMessage> {
    if message.subtype.is_some() {
        return None;
    }

    let text = message.content.as_ref()?.text.clone()?;
    let user_id = message.sender.user.as_ref()?.0.clone();
    let channel_id = message.origin.channel.as_ref()?.0.clone();

    Some(InboundMessage {
        platform: Platform::Slack,
        platform_msg_id: message.origin.ts.0.clone(),
        user: PlatformUser {
            platform_id: user_id.clone(),
            display_name: slack_sender_name(&message.sender),
            moa_user_id: None,
        },
        channel: slack_channel_ref(
            &channel_id,
            message.origin.thread_ts.as_ref().map(|ts| ts.0.as_str()),
            &user_id,
        ),
        text,
        attachments: Vec::new(),
        reply_to: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        timestamp: slack_ts_to_datetime(&message.origin.ts.0),
    })
}

fn slack_channel_ref(channel_id: &str, thread_ts: Option<&str>, user_id: &str) -> ChannelRef {
    if channel_id.starts_with('D') {
        return ChannelRef::DirectMessage {
            user_id: user_id.to_string(),
        };
    }

    if let Some(thread_ts) = thread_ts {
        return ChannelRef::Thread {
            channel_id: channel_id.to_string(),
            thread_id: thread_ts.to_string(),
        };
    }

    ChannelRef::Group {
        channel_id: channel_id.to_string(),
    }
}

fn slack_sender_name(sender: &SlackMessageSender) -> String {
    sender
        .username
        .clone()
        .or_else(|| sender.user.as_ref().map(|user| format!("<@{}>", user.0)))
        .unwrap_or_else(|| "Slack User".to_string())
}

fn slack_message_content(chunk: &SlackRenderChunk) -> SlackMessageContent {
    SlackMessageContent {
        text: Some(chunk.text.clone()),
        blocks: chunk.blocks.clone(),
        attachments: None,
        upload: None,
        files: None,
        reactions: None,
        metadata: None,
    }
}

fn slack_ts_to_datetime(value: &str) -> chrono::DateTime<Utc> {
    let seconds = value
        .split('.')
        .next()
        .and_then(|seconds| seconds.parse::<i64>().ok())
        .unwrap_or(0);
    chrono::DateTime::<Utc>::from_timestamp(seconds, 0).unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use slack_morphism::errors::{SlackClientApiError, SlackRateLimitError};

    #[test]
    fn parses_inbound_message_from_push_event() {
        let event: SlackPushEventCallback = serde_json::from_value(json!({
            "team_id": "T123",
            "api_app_id": "A123",
            "event": {
                "type": "message",
                "user": "U123",
                "text": "hello slack",
                "ts": "1712668800.000100",
                "channel": "D123",
                "channel_type": "im"
            },
            "event_id": "Ev123",
            "event_time": 1712668800
        }))
        .expect("slack push event should deserialize");

        let inbound = inbound_from_push_event(&event).expect("normalized slack event");
        assert_eq!(inbound.platform, Platform::Slack);
        assert_eq!(inbound.platform_msg_id, "1712668800.000100");
        assert_eq!(inbound.text, "hello slack");
        assert_eq!(
            inbound.channel,
            ChannelRef::DirectMessage {
                user_id: "U123".to_string()
            }
        );
    }

    #[tokio::test]
    async fn ignores_legacy_review_button_interactions() {
        let event: SlackInteractionEvent = serde_json::from_value(json!({
            "type": "block_actions",
            "team": { "id": "T123", "domain": "example" },
            "user": { "id": "U123", "username": "alice", "name": "Alice" },
            "api_app_id": "A123",
            "container": {
                "type": "message",
                "message_ts": "1712668800.000200",
                "channel_id": "C123"
            },
            "trigger_id": "1337.42.abcd",
            "channel": { "id": "C123", "name": "general" },
            "message": {
                "text": "action review",
                "ts": "1712668800.000200",
                "thread_ts": "1712668800.000050",
                "channel": "C123"
            },
            "actions": [{
                "type": "button",
                "action_id": "open",
                "value": "action_review:open"
            }]
        }))
        .expect("slack interaction should deserialize");

        let inbound = inbound_from_interaction_event(
            &event,
            Arc::new(RwLock::new(HashMap::<String, SlackMessageRef>::new())),
        )
        .await;

        assert_eq!(inbound, None);
    }

    #[test]
    fn slack_rate_limit_error_maps_to_typed_moa_rate_limit() {
        // Pins: Slack 429 errors stay typed after crossing the adapter boundary.
        let error = SlackClientError::RateLimitError(
            SlackRateLimitError::new()
                .with_retry_after(Duration::from_secs(5))
                .with_http_response_body("rate limited".to_string()),
        );
        let failure = classify_slack_client_error(&error);
        assert_eq!(failure.class, SlackApiFailureClass::Retryable);
        assert_eq!(failure.http_status, Some(429));
        assert_eq!(failure.retry_after, Some(Duration::from_secs(5)));

        let error = slack_client_error("chat.postMessage", error);
        assert!(matches!(
            error,
            MoaError::RateLimited {
                retries: SLACK_RATE_LIMIT_RETRIES,
                message
            } if message == "slack Web API rate limit was exceeded: rate limited"
        ));
    }

    #[test]
    fn slack_api_errors_are_classified_before_provider_mapping() {
        // Pins: Slack ok:false errors retain retryability rather than collapsing to opaque text.
        let retryable =
            SlackClientError::ApiError(SlackClientApiError::new("internal_error".to_string()));
        let retryable_failure = classify_slack_client_error(&retryable);
        assert_eq!(retryable_failure.class, SlackApiFailureClass::Retryable);
        assert!(retryable_failure.is_retryable());
        assert!(matches!(
            slack_client_error("chat.postMessage", retryable),
            MoaError::ProviderQuirk(message)
                if message == "slack Web API returned error code internal_error"
        ));

        let permanent =
            SlackClientError::ApiError(SlackClientApiError::new("invalid_auth".to_string()));
        let permanent_failure = classify_slack_client_error(&permanent);
        assert_eq!(permanent_failure.class, SlackApiFailureClass::Permanent);
        assert!(!permanent_failure.is_retryable());
        assert!(matches!(
            slack_client_error("chat.postMessage", permanent),
            MoaError::ProviderError(message)
                if message == "slack Web API returned error code invalid_auth"
        ));
    }
}
