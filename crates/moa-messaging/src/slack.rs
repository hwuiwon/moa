//! Slack channel adapter built on top of `slack-morphism` Socket Mode.

use std::{sync::Arc, time::Duration, time::Instant};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::traits::{ChannelAdapter, RuntimeCacheStore};
use moa_core::{
    Channel, ChannelActor, ChannelCapabilities, ChannelEvent, ChannelRef, ChannelSessionCommand,
    InboundMessage, MessageId, MoaConfig, MoaError, OutboundMessage, Result,
};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use slack_morphism::errors::SlackClientError;
use slack_morphism::prelude::*;
use tokio::{sync::mpsc, time::sleep};
use tracing::{Instrument, field, warn};
use uuid::Uuid;

use crate::{
    action_review::prepare_outbound_message,
    messaging_receive_span,
    rate_limit::MessagingRateLimiter,
    renderer::{SlackRenderChunk, SlackRenderer},
};

const SLACK_RATE_LIMIT_RETRIES: usize = 3;
const SLACK_MESSAGE_ID_PREFIX: &str = "slack:";
const SLACK_OUTBOUND_REF_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SLACK_OUTBOUND_REF_LOCK_TTL: Duration = Duration::from_secs(120);
const SLACK_OUTBOUND_REF_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const SLACK_OUTBOUND_REF_LOCK_RELEASE_TTL: Duration = Duration::from_millis(1);
/// Deadline for acquiring the shared outbound-ref update lock before giving up.
///
/// A crashed holder's lock auto-expires after [`SLACK_OUTBOUND_REF_LOCK_TTL`], so
/// waiting one lock TTL guarantees acquisition after a dead holder while bounding
/// how long an edit/delete can block on a live contender.
const SLACK_OUTBOUND_REF_LOCK_ACQUIRE_TIMEOUT: Duration = SLACK_OUTBOUND_REF_LOCK_TTL;
/// Upper bound on entries retained by each process-local Slack cache.
const SLACK_CACHE_MAX_CAPACITY: u64 = 100_000;
/// Retention for process-local inbound reply-context refs, matched to the shared
/// outbound-ref TTL so both sides of a conversation age out together.
const SLACK_INBOUND_CONTEXT_TTL: Duration = SLACK_OUTBOUND_REF_CACHE_TTL;
/// Retention for the last-edit timestamps used only for edit-interval logging.
const SLACK_LAST_EDIT_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
struct SlackListenerState {
    event_tx: mpsc::Sender<ChannelEvent>,
    inbound_contexts: Cache<String, SlackMessageRef>,
}

#[derive(Clone)]
struct SlackOutboundMessageRefs {
    hot_refs: Cache<String, Vec<SlackMessageRef>>,
    runtime_cache: Option<Arc<dyn RuntimeCacheStore>>,
}

struct SlackOutboundRefUpdateLock {
    runtime_cache: Arc<dyn RuntimeCacheStore>,
    key: String,
    token: Vec<u8>,
}

impl SlackOutboundRefUpdateLock {
    async fn release(self) -> Result<()> {
        self.runtime_cache
            .compare_and_set(
                &self.key,
                Some(&self.token),
                b"released".to_vec(),
                SLACK_OUTBOUND_REF_LOCK_RELEASE_TTL,
            )
            .await?;
        Ok(())
    }
}

impl SlackOutboundMessageRefs {
    fn new(runtime_cache: Option<Arc<dyn RuntimeCacheStore>>) -> Self {
        Self {
            hot_refs: Cache::builder()
                .max_capacity(SLACK_CACHE_MAX_CAPACITY)
                .time_to_live(SLACK_OUTBOUND_REF_CACHE_TTL)
                .build(),
            runtime_cache,
        }
    }

    async fn resolve(&self, msg_id: &MessageId) -> Result<Vec<SlackMessageRef>> {
        if let Some(refs) = self.load(msg_id).await? {
            return Ok(refs);
        }

        let Some(message_ref) = slack_message_ref_from_id(msg_id) else {
            return Err(MoaError::ValidationError(format!(
                "unknown slack message id: {msg_id}"
            )));
        };
        let refs = vec![message_ref];
        self.refresh_shared_refs_best_effort(msg_id, &refs, "resolve_single_ref")
            .await;
        Ok(refs)
    }

    #[cfg(test)]
    async fn store(&self, msg_id: &MessageId, refs: Vec<SlackMessageRef>) -> Result<()> {
        self.remember_hot_refs(msg_id, &refs).await;
        self.write_shared_refs(msg_id, &refs).await?;

        Ok(())
    }

    async fn record_after_external_side_effect(
        &self,
        msg_id: &MessageId,
        refs: Vec<SlackMessageRef>,
        operation: &'static str,
    ) {
        self.remember_hot_refs(msg_id, &refs).await;
        self.refresh_shared_refs_best_effort(msg_id, &refs, operation)
            .await;
    }

    async fn load(&self, msg_id: &MessageId) -> Result<Option<Vec<SlackMessageRef>>> {
        if let Some(runtime_cache) = &self.runtime_cache {
            let key = slack_outbound_refs_cache_key(msg_id);
            match runtime_cache.get(&key).await {
                Ok(Some(value)) => {
                    let refs = decode_slack_message_refs(&value)?;
                    self.remember_hot_refs(msg_id, &refs).await;
                    if let Err(error) = runtime_cache
                        .expire(&key, SLACK_OUTBOUND_REF_CACHE_TTL)
                        .await
                    {
                        warn!(
                            message_id = %msg_id,
                            error = %error,
                            "slack outbound refs loaded but shared TTL refresh failed"
                        );
                    }
                    return Ok(Some(refs));
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(refs) = self.hot_refs.get(msg_id.as_str()).await {
                        warn!(
                            message_id = %msg_id,
                            error = %error,
                            "using process-local Slack outbound refs after shared cache read failed"
                        );
                        return Ok(Some(refs));
                    }
                    return Err(error);
                }
            }
        }

        let refs = self.hot_refs.get(msg_id.as_str()).await;
        if let Some(refs) = refs {
            self.refresh_shared_refs_best_effort(msg_id, &refs, "load_hot_refs")
                .await;
            return Ok(Some(refs));
        }

        Ok(None)
    }

    async fn remove_after_external_side_effect(&self, msg_id: &MessageId, operation: &'static str) {
        self.hot_refs.invalidate(msg_id.as_str()).await;
        if let Some(runtime_cache) = &self.runtime_cache
            && let Err(error) = runtime_cache
                .delete(&slack_outbound_refs_cache_key(msg_id))
                .await
        {
            warn!(
                message_id = %msg_id,
                operation,
                error = %error,
                "Slack accepted external side effect but shared outbound ref cleanup failed"
            );
        }
    }

    async fn acquire_update_lock(
        &self,
        msg_id: &MessageId,
    ) -> Result<Option<SlackOutboundRefUpdateLock>> {
        let Some(runtime_cache) = &self.runtime_cache else {
            return Ok(None);
        };
        let key = slack_outbound_refs_lock_key(msg_id);
        let token = Uuid::now_v7().to_string().into_bytes();
        // The lock is held across the full edit/delete Slack API sequence to
        // serialize concurrent ref mutations for one message, so its scope is
        // kept as-is; only acquisition is bounded so an edit/delete cannot block
        // forever on a live contender or a lost lock release.
        let deadline = Instant::now() + SLACK_OUTBOUND_REF_LOCK_ACQUIRE_TIMEOUT;

        loop {
            let current = runtime_cache.get(&key).await?;
            if current.is_none()
                && runtime_cache
                    .compare_and_set(&key, None, token.clone(), SLACK_OUTBOUND_REF_LOCK_TTL)
                    .await?
            {
                return Ok(Some(SlackOutboundRefUpdateLock {
                    runtime_cache: runtime_cache.clone(),
                    key,
                    token,
                }));
            }

            if Instant::now() >= deadline {
                return Err(MoaError::ProviderQuirk(format!(
                    "timed out acquiring Slack outbound ref update lock for {msg_id}"
                )));
            }
            sleep(crate::rate_limit::with_jitter(
                SLACK_OUTBOUND_REF_LOCK_RETRY_INTERVAL,
            ))
            .await;
        }
    }

    async fn release_update_lock(&self, update_lock: Option<SlackOutboundRefUpdateLock>) {
        let Some(update_lock) = update_lock else {
            return;
        };
        if let Err(error) = update_lock.release().await {
            warn!(
                error = %error,
                "Slack outbound ref update lock release failed"
            );
        }
    }

    async fn remember_hot_refs(&self, msg_id: &MessageId, refs: &[SlackMessageRef]) {
        self.hot_refs
            .insert(msg_id.as_str().to_string(), refs.to_vec())
            .await;
    }

    async fn write_shared_refs(&self, msg_id: &MessageId, refs: &[SlackMessageRef]) -> Result<()> {
        let Some(runtime_cache) = &self.runtime_cache else {
            return Ok(());
        };
        runtime_cache
            .set(
                &slack_outbound_refs_cache_key(msg_id),
                encode_slack_message_refs(refs)?,
                SLACK_OUTBOUND_REF_CACHE_TTL,
            )
            .await?;
        Ok(())
    }

    async fn refresh_shared_refs_best_effort(
        &self,
        msg_id: &MessageId,
        refs: &[SlackMessageRef],
        operation: &'static str,
    ) {
        if let Err(error) = self.write_shared_refs(msg_id, refs).await {
            warn!(
                message_id = %msg_id,
                operation,
                error = %error,
                "Slack outbound ref storage failed; using process-local refs when available"
            );
        }
    }
}

/// Slack adapter implementing the generic channel abstraction.
#[derive(Clone)]
pub struct SlackAdapter {
    client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    bot_token: SlackApiToken,
    app_token: SlackApiToken,
    renderer: SlackRenderer,
    inbound_contexts: Cache<String, SlackMessageRef>,
    outbound_refs: SlackOutboundMessageRefs,
    rate_limiter: MessagingRateLimiter,
    last_edits: Cache<String, Instant>,
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
            inbound_contexts: Cache::builder()
                .max_capacity(SLACK_CACHE_MAX_CAPACITY)
                .time_to_live(SLACK_INBOUND_CONTEXT_TTL)
                .build(),
            outbound_refs: SlackOutboundMessageRefs::new(None),
            rate_limiter: MessagingRateLimiter::for_channel(Channel::Slack),
            last_edits: Cache::builder()
                .max_capacity(SLACK_CACHE_MAX_CAPACITY)
                .time_to_live(SLACK_LAST_EDIT_TTL)
                .build(),
        })
    }

    /// Uses a runtime cache for Slack pacing and outbound message references.
    ///
    /// Redis-backed stores coordinate across replicas. The memory backend is
    /// process-local and only coordinates adapters that share the same store
    /// instance, so it is a local-development best-effort fallback.
    #[must_use]
    pub fn with_runtime_cache(mut self, runtime_cache: Arc<dyn RuntimeCacheStore>) -> Self {
        self.rate_limiter = self.rate_limiter.with_runtime_cache(runtime_cache.clone());
        self.outbound_refs.runtime_cache = Some(runtime_cache);
        self
    }

    /// Creates a Slack adapter using the configured token environment variables.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let bot_token = moa_core::config::required_config_secret(
            "MOA_MESSAGING_SLACK_TOKEN",
            &config.messaging.slack_token,
        )?;
        let app_token = moa_core::config::required_config_secret(
            "MOA_MESSAGING_SLACK_APP_TOKEN",
            &config.messaging.slack_app_token,
        )?;
        Self::new(bot_token, app_token)
    }

    /// Creates a Slack adapter using configured tokens and a shared runtime cache.
    pub fn from_config_with_runtime_cache(
        config: &MoaConfig,
        runtime_cache: Arc<dyn RuntimeCacheStore>,
    ) -> Result<Self> {
        Self::from_config(config).map(|adapter| adapter.with_runtime_cache(runtime_cache))
    }

    async fn resolve_target(
        &self,
        reply_to: Option<&str>,
        channel_ref: Option<&ChannelRef>,
    ) -> Result<SlackTarget> {
        if let Some(reply_to) = reply_to {
            let reply_to_id = MessageId::new(reply_to.to_string());
            if let Some(last_ref) = self
                .outbound_refs
                .load(&reply_to_id)
                .await?
                .and_then(|refs| refs.last().cloned())
            {
                return Ok(last_ref.target());
            }

            if let Some(inbound_ref) = self.inbound_contexts.get(reply_to).await {
                return Ok(inbound_ref.target());
            }
        }

        if let Some(target) = channel_ref.and_then(slack_target_from_channel_ref) {
            return Ok(target);
        }

        Err(MoaError::ValidationError(
            "slack outbound messages require reply_to context or Slack channel_ref".into(),
        ))
    }

    async fn send_chunk(
        &self,
        target: &SlackTarget,
        chunk: &SlackRenderChunk,
    ) -> Result<SlackMessageRef> {
        self.rate_limiter
            .wait_for_channel_slot(target.channel_id.as_ref())
            .await?;
        let session = self.client.open_session(&self.bot_token);
        let request = SlackApiChatPostMessageRequest {
            channel: SlackChannelId(target.channel_id.to_string()),
            content: slack_message_content(chunk),
            as_user: None,
            icon_emoji: None,
            icon_url: None,
            link_names: None,
            parse: None,
            thread_ts: target.thread_ts.clone().map(SlackTs),
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
                thread_ts: target.thread_ts.clone(),
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
        self.rate_limiter
            .wait_for_channel_slot(message_ref.channel_id.as_ref())
            .await?;
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

    async fn record_edit_attempt(&self, message_id: &MessageId) {
        let min_interval = self.capabilities().min_edit_interval;
        let previous = self.last_edits.get(message_id.as_str()).await;
        self.last_edits
            .insert(message_id.as_str().to_string(), Instant::now())
            .await;
        if let Some(last_edit) = previous {
            let remaining = min_interval.saturating_sub(last_edit.elapsed());
            if !remaining.is_zero() {
                tracing::debug!(
                    message_id = %message_id,
                    remaining_ms = remaining.as_millis() as u64,
                    "slack edit requested before advertised edit interval elapsed"
                );
            }
        }
    }

    #[cfg(test)]
    async fn test_store_outbound_refs(
        &self,
        msg_id: &MessageId,
        refs: Vec<SlackMessageRef>,
    ) -> Result<()> {
        self.outbound_refs.store(msg_id, refs).await
    }

    #[cfg(test)]
    async fn test_resolve_outbound_refs(&self, msg_id: &MessageId) -> Result<Vec<SlackMessageRef>> {
        self.outbound_refs.resolve(msg_id).await
    }

    #[cfg(test)]
    async fn test_remove_outbound_refs_after_external_side_effect(&self, msg_id: &MessageId) {
        self.outbound_refs
            .remove_after_external_side_effect(msg_id, "test")
            .await
    }
}

/// Normalizes one Slack Events API callback JSON payload into MOA's canonical channel event shape.
pub fn normalize_event_json(payload: &str) -> Result<ChannelEvent> {
    let event: SlackPushEventCallback = serde_json::from_str(payload)?;
    normalize_push_event(&event)
}

/// Normalizes one parsed Slack push event into MOA's canonical channel event shape.
pub fn normalize_push_event(event: &SlackPushEventCallback) -> Result<ChannelEvent> {
    inbound_from_push_event(event)
        .ok_or_else(|| {
            MoaError::ValidationError("slack event is not a supported user message".to_string())
        })
        .map(channel_event_from_inbound)
}

#[async_trait]
impl ChannelAdapter for SlackAdapter {
    /// Returns the adapter channel identifier.
    fn channel(&self) -> Channel {
        self.renderer.channel()
    }

    /// Returns Slack transport capabilities.
    fn capabilities(&self) -> ChannelCapabilities {
        self.renderer.capabilities()
    }

    /// Starts the Slack Socket Mode listener and forwards normalized updates.
    async fn start(&self, event_tx: mpsc::Sender<ChannelEvent>) -> Result<()> {
        let client = self.client.clone();
        let callbacks = SlackSocketModeListenerCallbacks::new().with_push_events(handle_push_event);

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
        let msg = prepare_outbound_message(self.channel(), &self.capabilities(), msg);
        let target = self
            .resolve_target(msg.reply_to.as_deref(), msg.channel_ref.as_ref())
            .await?;
        let rendered = self.renderer.render(&msg);
        let mut sent_refs = Vec::with_capacity(rendered.len());
        for chunk in &rendered {
            let sent_ref = self.send_chunk(&target, chunk).await?;
            sent_refs.push(sent_ref);
        }
        let message_id = if sent_refs.len() == 1 {
            slack_message_id_from_ref(&sent_refs[0])
        } else {
            MessageId::new(Uuid::now_v7().to_string())
        };
        self.outbound_refs
            .record_after_external_side_effect(&message_id, sent_refs, "chat.postMessage")
            .await;
        Ok(message_id)
    }

    /// Edits an existing outbound Slack message in place.
    async fn edit(&self, msg_id: &MessageId, msg: OutboundMessage) -> Result<()> {
        let update_lock = self.outbound_refs.acquire_update_lock(msg_id).await?;
        let result = self.edit_locked(msg_id, msg).await;
        self.outbound_refs.release_update_lock(update_lock).await;
        result
    }

    /// Deletes a Slack message sent through this adapter.
    async fn delete(&self, msg_id: &MessageId) -> Result<()> {
        let update_lock = self.outbound_refs.acquire_update_lock(msg_id).await?;
        let result = self.delete_locked(msg_id).await;
        self.outbound_refs.release_update_lock(update_lock).await;
        result
    }
}

impl SlackAdapter {
    async fn edit_locked(&self, msg_id: &MessageId, msg: OutboundMessage) -> Result<()> {
        self.record_edit_attempt(msg_id).await;
        let msg = prepare_outbound_message(self.channel(), &self.capabilities(), msg);

        let existing = self.outbound_refs.resolve(msg_id).await?;
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
                self.rate_limiter
                    .wait_for_channel_slot(message_ref.channel_id.as_ref())
                    .await?;
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

        self.outbound_refs
            .record_after_external_side_effect(msg_id, updated_refs, "chat.update")
            .await;
        Ok(())
    }

    async fn delete_locked(&self, msg_id: &MessageId) -> Result<()> {
        let refs = self.outbound_refs.resolve(msg_id).await?;
        let session = self.client.open_session(&self.bot_token);
        for message_ref in refs {
            self.rate_limiter
                .wait_for_channel_slot(message_ref.channel_id.as_ref())
                .await?;
            let request = SlackApiChatDeleteRequest {
                channel: SlackChannelId(message_ref.channel_id.to_string()),
                ts: SlackTs(message_ref.ts),
                as_user: None,
            };
            match session.chat_delete(&request).await {
                Ok(_) => {}
                Err(SlackClientError::ApiError(api)) if api.code == "message_not_found" => {
                    tracing::debug!(
                        message_id = %msg_id,
                        slack.channel_id = %message_ref.channel_id,
                        slack.ts = %request.ts.0,
                        "Slack delete retried a chunk that was already absent"
                    );
                }
                Err(error) => return Err(slack_client_error("chat.delete", error)),
            }
        }
        self.outbound_refs
            .remove_after_external_side_effect(msg_id, "chat.delete")
            .await;
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
        let channel_event = channel_event_from_inbound(inbound);
        let inbound = inbound_for_event(&channel_event);
        let channel_msg_id = inbound.channel_msg_id.clone();
        let messaging_span = messaging_receive_span(inbound);
        async move {
            if let Some(origin) = push_event_origin(&event) {
                shared.inbound_contexts.insert(channel_msg_id, origin).await;
            }
            if shared.event_tx.send(channel_event).await.is_err() {
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
    thread_ts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlackMessageRef {
    channel_id: Arc<str>,
    ts: String,
    thread_ts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SlackMessageRefRecord {
    channel_id: String,
    ts: String,
    thread_ts: Option<String>,
}

impl SlackMessageRef {
    fn target(&self) -> SlackTarget {
        SlackTarget {
            channel_id: self.channel_id.clone(),
            thread_ts: Some(self.thread_anchor().to_string()),
        }
    }

    fn thread_anchor(&self) -> &str {
        self.thread_ts.as_deref().unwrap_or(self.ts.as_str())
    }
}

impl From<&SlackMessageRef> for SlackMessageRefRecord {
    fn from(message_ref: &SlackMessageRef) -> Self {
        Self {
            channel_id: message_ref.channel_id.to_string(),
            ts: message_ref.ts.clone(),
            thread_ts: message_ref.thread_ts.clone(),
        }
    }
}

impl From<SlackMessageRefRecord> for SlackMessageRef {
    fn from(record: SlackMessageRefRecord) -> Self {
        Self {
            channel_id: Arc::<str>::from(record.channel_id),
            ts: record.ts,
            thread_ts: record.thread_ts,
        }
    }
}

fn encode_slack_message_refs(refs: &[SlackMessageRef]) -> Result<Vec<u8>> {
    let records: Vec<SlackMessageRefRecord> =
        refs.iter().map(SlackMessageRefRecord::from).collect();
    Ok(serde_json::to_vec(&records)?)
}

fn decode_slack_message_refs(value: &[u8]) -> Result<Vec<SlackMessageRef>> {
    let records: Vec<SlackMessageRefRecord> = serde_json::from_slice(value)?;
    Ok(records.into_iter().map(SlackMessageRef::from).collect())
}

fn slack_outbound_refs_cache_key(message_id: &MessageId) -> String {
    format!("moa:messaging:slack:outbound_refs:{message_id}")
}

fn slack_outbound_refs_lock_key(message_id: &MessageId) -> String {
    format!("moa:messaging:slack:outbound_refs_lock:{message_id}")
}

fn slack_message_id_from_ref(message_ref: &SlackMessageRef) -> MessageId {
    MessageId::new(format!(
        "{SLACK_MESSAGE_ID_PREFIX}{}:{}",
        message_ref.channel_id, message_ref.ts
    ))
}

fn slack_message_ref_from_id(message_id: &MessageId) -> Option<SlackMessageRef> {
    let value = message_id.as_str().strip_prefix(SLACK_MESSAGE_ID_PREFIX)?;
    let (channel_id, ts) = value.split_once(':')?;
    if channel_id.is_empty() || ts.is_empty() {
        return None;
    }
    Some(SlackMessageRef {
        channel_id: Arc::<str>::from(channel_id),
        ts: ts.to_string(),
        thread_ts: None,
    })
}

fn slack_target_from_channel_ref(channel_ref: &ChannelRef) -> Option<SlackTarget> {
    let ChannelRef::Slack {
        slack_channel_id,
        thread_ts,
        ..
    } = channel_ref
    else {
        return None;
    };
    Some(SlackTarget {
        channel_id: Arc::<str>::from(slack_channel_id.as_ref()?.as_str()),
        thread_ts: thread_ts.clone(),
    })
}

fn inbound_from_push_event(event: &SlackPushEventCallback) -> Option<InboundMessage> {
    let team_id = Some(event.team_id.as_ref());
    match &event.event {
        SlackEventCallbackBody::AppMention(message) => inbound_from_app_mention(message, team_id),
        SlackEventCallbackBody::Message(message) => inbound_from_message_event(message, team_id),
        _ => None,
    }
}

fn channel_event_from_inbound(inbound: InboundMessage) -> ChannelEvent {
    match parse_session_command(&inbound.text) {
        Some(SlackSessionCommand::Status) => {
            ChannelEvent::SessionCommand(ChannelSessionCommand::Status(inbound))
        }
        Some(SlackSessionCommand::Stop) => {
            ChannelEvent::SessionCommand(ChannelSessionCommand::Stop(inbound))
        }
        None => ChannelEvent::Message(inbound),
    }
}

fn inbound_for_event(event: &ChannelEvent) -> &InboundMessage {
    match event {
        ChannelEvent::Message(inbound) => inbound,
        ChannelEvent::SessionCommand(ChannelSessionCommand::Status(inbound))
        | ChannelEvent::SessionCommand(ChannelSessionCommand::Stop(inbound)) => inbound,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlackSessionCommand {
    Status,
    Stop,
}

fn parse_session_command(text: &str) -> Option<SlackSessionCommand> {
    match text.trim() {
        "/moa status" => Some(SlackSessionCommand::Status),
        "/moa stop" => Some(SlackSessionCommand::Stop),
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

fn inbound_from_app_mention(
    message: &SlackAppMentionEvent,
    team_id: Option<&str>,
) -> Option<InboundMessage> {
    let text = message.content.text.clone()?;
    let channel_msg_id = message.origin.ts.0.clone();
    let user_id = message.user.0.clone();
    Some(InboundMessage {
        channel: Channel::Slack,
        channel_msg_id,
        actor: ChannelActor {
            external_id: user_id.clone(),
            display_name: format!("<@{}>", message.user.0),
            channel_account_id: None,
            moa_user_id: None,
        },
        channel_ref: slack_channel_ref(
            team_id,
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

fn inbound_from_message_event(
    message: &SlackMessageEvent,
    team_id: Option<&str>,
) -> Option<InboundMessage> {
    if message.subtype.is_some() {
        return None;
    }

    let text = message.content.as_ref()?.text.clone()?;
    let user_id = message.sender.user.as_ref()?.0.clone();
    let channel_id = message.origin.channel.as_ref()?.0.clone();

    Some(InboundMessage {
        channel: Channel::Slack,
        channel_msg_id: message.origin.ts.0.clone(),
        actor: ChannelActor {
            external_id: user_id.clone(),
            display_name: slack_sender_name(&message.sender),
            channel_account_id: None,
            moa_user_id: None,
        },
        channel_ref: slack_channel_ref(
            team_id,
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

fn slack_channel_ref(
    team_id: Option<&str>,
    channel_id: &str,
    thread_ts: Option<&str>,
    user_id: &str,
) -> ChannelRef {
    ChannelRef::Slack {
        team_id: team_id.map(ToOwned::to_owned),
        slack_channel_id: Some(channel_id.to_string()),
        // Direct-message channels never thread their replies; channel posts carry
        // the originating thread when one is present.
        thread_ts: if channel_id.starts_with('D') {
            None
        } else {
            thread_ts.map(ToOwned::to_owned)
        },
        user_id: Some(user_id.to_string()),
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
    use async_trait::async_trait;
    use moa_runtime_store::MemoryRuntimeCacheStore;
    use serde_json::json;
    use slack_morphism::errors::{SlackClientApiError, SlackRateLimitError};
    use tokio::time::timeout;

    #[derive(Debug)]
    struct FailingSetRuntimeCacheStore;

    #[async_trait]
    impl RuntimeCacheStore for FailingSetRuntimeCacheStore {
        async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }

        async fn set(&self, _key: &str, _value: Vec<u8>, _ttl: Duration) -> Result<()> {
            Err(MoaError::StorageError(
                "runtime cache set failed".to_string(),
            ))
        }

        async fn delete(&self, _key: &str) -> Result<()> {
            Err(MoaError::StorageError(
                "runtime cache delete failed".to_string(),
            ))
        }

        async fn compare_and_set(
            &self,
            _key: &str,
            _expected: Option<&[u8]>,
            _value: Vec<u8>,
            _ttl: Duration,
        ) -> Result<bool> {
            Err(MoaError::StorageError(
                "runtime cache CAS failed".to_string(),
            ))
        }

        async fn expire(&self, _key: &str, _ttl: Duration) -> Result<()> {
            Err(MoaError::StorageError(
                "runtime cache expire failed".to_string(),
            ))
        }
    }

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
        assert_eq!(inbound.channel, Channel::Slack);
        assert_eq!(inbound.channel_msg_id, "1712668800.000100");
        assert_eq!(inbound.text, "hello slack");
        assert_eq!(
            inbound.channel_ref,
            ChannelRef::Slack {
                team_id: Some("T123".to_string()),
                slack_channel_id: Some("D123".to_string()),
                thread_ts: None,
                user_id: Some("U123".to_string())
            }
        );
    }

    #[test]
    fn parses_inbound_message_from_app_mention_event() {
        // Pins: an `app_mention` callback flows through the AppMention branch of
        // inbound_from_push_event and normalizes to a canonical inbound message.
        let event: SlackPushEventCallback = serde_json::from_value(json!({
            "team_id": "T123",
            "api_app_id": "A123",
            "event": {
                "type": "app_mention",
                "user": "U123",
                "text": "<@U999> please summarize this",
                "ts": "1712668800.000100",
                "channel": "C123",
                "event_ts": "1712668800.000100"
            },
            "event_id": "Ev123",
            "event_time": 1712668800
        }))
        .expect("slack app_mention event should deserialize");

        let inbound =
            inbound_from_push_event(&event).expect("app_mention should normalize to inbound");
        assert_eq!(inbound.channel, Channel::Slack);
        assert_eq!(inbound.channel_msg_id, "1712668800.000100");
        assert_eq!(inbound.text, "<@U999> please summarize this");
        assert_eq!(inbound.actor.external_id, "U123");
        assert_eq!(inbound.actor.display_name, "<@U123>");
        assert_eq!(
            inbound.channel_ref,
            ChannelRef::Slack {
                team_id: Some("T123".to_string()),
                slack_channel_id: Some("C123".to_string()),
                thread_ts: None,
                user_id: Some("U123".to_string())
            }
        );
    }

    #[test]
    fn message_event_with_subtype_is_filtered_to_none() {
        // Pins: a `message` event carrying a subtype (e.g. bot_message / message_changed) is
        // dropped by inbound_from_message_event's subtype filter even though it is otherwise a
        // fully-formed, normalizable message (text + user + channel all present).
        let event: SlackPushEventCallback = serde_json::from_value(json!({
            "team_id": "T123",
            "api_app_id": "A123",
            "event": {
                "type": "message",
                "subtype": "bot_message",
                "user": "U123",
                "text": "posted by a bot integration",
                "ts": "1712668800.000100",
                "channel": "C123",
                "channel_type": "channel"
            },
            "event_id": "Ev123",
            "event_time": 1712668800
        }))
        .expect("slack message event with subtype should deserialize");

        assert!(
            inbound_from_push_event(&event).is_none(),
            "message events carrying a subtype must be filtered out, not normalized"
        );
    }

    #[test]
    fn slack_target_uses_durable_channel_ref_when_reply_anchor_is_absent() {
        // Pins: workflow-originated progress can send after process restart using the persisted route.
        let target = slack_target_from_channel_ref(&ChannelRef::Slack {
            team_id: Some("T123".to_string()),
            slack_channel_id: Some("C123".to_string()),
            thread_ts: Some("1712668800.000100".to_string()),
            user_id: Some("U123".to_string()),
        })
        .expect("slack route with channel id should resolve");

        assert_eq!(target.channel_id.as_ref(), "C123");
        assert_eq!(target.thread_ts.as_deref(), Some("1712668800.000100"));
        assert!(
            slack_target_from_channel_ref(&ChannelRef::Chat {
                conversation_id: "chat-1".to_string(),
                user_id: None,
                client_session_id: None,
            })
            .is_none()
        );
    }

    #[test]
    fn slack_message_id_round_trips_single_message_ref() {
        // Pins: single-message Slack sends can be edited after worker restart without process-local state.
        let message_ref = SlackMessageRef {
            channel_id: Arc::<str>::from("C123"),
            ts: "1712668800.000100".to_string(),
            thread_ts: None,
        };

        let message_id = slack_message_id_from_ref(&message_ref);
        let restored =
            slack_message_ref_from_id(&message_id).expect("durable Slack message id should parse");

        assert_eq!(message_id.as_str(), "slack:C123:1712668800.000100");
        assert_eq!(restored.channel_id.as_ref(), "C123");
        assert_eq!(restored.ts, "1712668800.000100");
        assert_eq!(restored.thread_ts, None);
        assert!(slack_message_ref_from_id(&MessageId::new("other-id")).is_none());
    }

    #[tokio::test]
    async fn slack_multi_chunk_refs_survive_adapter_instance_boundaries_with_runtime_cache() {
        // Pins: multi-chunk Slack edit/delete continuity does not depend on one adapter instance.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let cache = Arc::new(MemoryRuntimeCacheStore::new());
        let first = SlackAdapter::new("xoxb-first", "xapp-first")
            .expect("setup: first Slack adapter should construct")
            .with_runtime_cache(cache.clone());
        let second = SlackAdapter::new("xoxb-second", "xapp-second")
            .expect("setup: second Slack adapter should construct")
            .with_runtime_cache(cache);
        let message_id = MessageId::new("multi-chunk-message");
        let refs = vec![
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668800.000100".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668801.000200".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
        ];

        first
            .test_store_outbound_refs(&message_id, refs.clone())
            .await
            .expect("first adapter state should write refs to runtime cache");

        assert_eq!(
            second
                .test_resolve_outbound_refs(&message_id)
                .await
                .expect("second adapter state should load refs from runtime cache"),
            refs
        );
        assert_eq!(
            second
                .test_resolve_outbound_refs(&message_id)
                .await
                .expect("edit/delete should resolve shared refs")
                .last()
                .expect("refs should contain the last chunk")
                .target(),
            SlackTarget {
                channel_id: Arc::<str>::from("C123"),
                thread_ts: Some("1712668800.000100".to_string()),
            }
        );
        assert_eq!(
            second
                .test_resolve_outbound_refs(&message_id)
                .await
                .expect("refs should remain available until Slack delete succeeds"),
            refs
        );
        second
            .test_remove_outbound_refs_after_external_side_effect(&message_id)
            .await;
        assert!(
            second
                .test_resolve_outbound_refs(&message_id)
                .await
                .is_err(),
            "second adapter should not resolve refs after successful Slack delete cleanup"
        );
    }

    #[tokio::test]
    async fn slack_ref_storage_failure_after_side_effect_keeps_local_refs() {
        // Pins: Slack send/edit success is not reported as failed solely because shared ref storage failed.
        let refs = SlackOutboundMessageRefs::new(Some(Arc::new(FailingSetRuntimeCacheStore)));
        let message_id = MessageId::new("multi-chunk-side-effect");
        let sent_refs = vec![
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668800.000100".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668801.000200".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
        ];

        refs.record_after_external_side_effect(&message_id, sent_refs.clone(), "chat.postMessage")
            .await;

        assert_eq!(
            refs.resolve(&message_id)
                .await
                .expect("process-local refs should remain after shared storage failure"),
            sent_refs
        );
    }

    #[tokio::test]
    async fn slack_hot_multi_chunk_refs_refresh_shared_cache_after_ttl_loss() {
        // Pins: loading hot multi-chunk refs refreshes shared refs instead of degrading to one chunk.
        let cache = Arc::new(MemoryRuntimeCacheStore::new());
        let first = SlackOutboundMessageRefs::new(Some(cache.clone()));
        let second = SlackOutboundMessageRefs::new(Some(cache.clone()));
        let message_id = MessageId::new("multi-chunk-refresh");
        let refs = vec![
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668800.000100".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668801.000200".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
        ];

        first
            .store(&message_id, refs.clone())
            .await
            .expect("setup: refs should write to shared cache");
        cache
            .delete(&slack_outbound_refs_cache_key(&message_id))
            .await
            .expect("setup: deleting cache key simulates TTL expiration");

        assert_eq!(
            first
                .resolve(&message_id)
                .await
                .expect("hot refs should resolve after shared TTL loss"),
            refs
        );
        assert_eq!(
            second
                .resolve(&message_id)
                .await
                .expect("hot load should refresh shared cache for other adapters"),
            refs
        );
    }

    #[tokio::test]
    async fn slack_shared_update_lock_serializes_cross_instance_ref_updates() {
        // Pins: cross-pod Slack edits cannot race shared ref updates for the same message id.
        let cache = Arc::new(MemoryRuntimeCacheStore::new());
        let first = SlackOutboundMessageRefs::new(Some(cache.clone()));
        let second = SlackOutboundMessageRefs::new(Some(cache));
        let message_id = MessageId::new("multi-chunk-lock");
        let initial_refs = vec![
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668800.000100".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668801.000200".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
        ];
        let expanded_refs = vec![
            initial_refs[0].clone(),
            initial_refs[1].clone(),
            SlackMessageRef {
                channel_id: Arc::<str>::from("C123"),
                ts: "1712668802.000300".to_string(),
                thread_ts: Some("1712668800.000100".to_string()),
            },
        ];

        first
            .store(&message_id, initial_refs)
            .await
            .expect("setup: initial refs should write to shared cache");
        let first_lock = first
            .acquire_update_lock(&message_id)
            .await
            .expect("first edit should acquire the shared ref lock");

        assert!(
            timeout(
                SLACK_OUTBOUND_REF_LOCK_RETRY_INTERVAL / 2,
                second.acquire_update_lock(&message_id)
            )
            .await
            .is_err(),
            "second edit acquired the Slack ref lock while the first edit still held it"
        );

        first
            .record_after_external_side_effect(&message_id, expanded_refs.clone(), "chat.update")
            .await;
        first.release_update_lock(first_lock).await;
        let second_lock = second
            .acquire_update_lock(&message_id)
            .await
            .expect("second edit should acquire the lock after release");
        assert_eq!(
            second
                .resolve(&message_id)
                .await
                .expect("second edit should load refs written by the first edit"),
            expanded_refs
        );
        second.release_update_lock(second_lock).await;
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
