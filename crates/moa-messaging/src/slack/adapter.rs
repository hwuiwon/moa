//! Slack channel adapter and Web API transport implementation.

use std::{sync::Arc, time::Duration, time::Instant};

use async_trait::async_trait;
use moa_core::traits::{ChannelAdapter, RuntimeCacheStore};
use moa_core::{
    config::MoaConfig, error::MoaError, error::Result, types::channel::Channel,
    types::channel::ChannelCapabilities, types::channel::ChannelEvent, types::channel::ChannelRef,
    types::channel::MessageId, types::channel::OutboundMessage,
};
use moka::future::Cache;
use slack_morphism::errors::SlackClientError;
use slack_morphism::prelude::*;
use tokio::sync::mpsc;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    action_review::prepare_outbound_message,
    rate_limit::MessagingRateLimiter,
    renderer::{SlackRenderChunk, SlackRenderer},
};

use super::chunking::{SlackChunkTransport, apply_edit_tracked, send_multi_chunk_tracked};
use super::error::{SLACK_RATE_LIMIT_RETRIES, slack_api_span, slack_client_error};
use super::inbound::{SlackListenerState, handle_push_event};
use super::refs::{
    SlackMessageRef, SlackOutboundMessageRefs, SlackTarget, slack_message_id_from_ref,
    slack_target_from_channel_ref,
};

const SLACK_CACHE_MAX_CAPACITY: u64 = 100_000;
const SLACK_INBOUND_CONTEXT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SLACK_LAST_EDIT_TTL: Duration = Duration::from_secs(60 * 60);

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
    pub(super) async fn test_store_outbound_refs(
        &self,
        msg_id: &MessageId,
        refs: Vec<SlackMessageRef>,
    ) -> Result<()> {
        self.outbound_refs.store(msg_id, refs).await
    }

    #[cfg(test)]
    pub(super) async fn test_resolve_outbound_refs(
        &self,
        msg_id: &MessageId,
    ) -> Result<Vec<SlackMessageRef>> {
        self.outbound_refs.resolve(msg_id).await
    }

    #[cfg(test)]
    pub(super) async fn test_remove_outbound_refs_after_external_side_effect(
        &self,
        msg_id: &MessageId,
    ) {
        self.outbound_refs
            .remove_after_external_side_effect(msg_id, "test")
            .await
    }
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
        let msg = prepare_outbound_message(msg);
        let target = self
            .resolve_target(msg.reply_to.as_deref(), msg.channel_ref.as_ref())
            .await?;
        let rendered = self.renderer.render(&msg);

        // A single-chunk (or empty) send has no partial-visibility window: the one
        // send either fully succeeds or leaves nothing visible. Keep the
        // deterministic id derived from the sole reference.
        if rendered.len() <= 1 {
            let mut sent_refs = Vec::with_capacity(rendered.len());
            for chunk in &rendered {
                sent_refs.push(self.send_chunk(&target, chunk).await?);
            }
            let message_id = sent_refs
                .first()
                .map(slack_message_id_from_ref)
                .unwrap_or_else(|| MessageId::new(Uuid::now_v7().to_string()));
            self.outbound_refs
                .record_after_external_side_effect(&message_id, sent_refs, "chat.postMessage")
                .await;
            return Ok(message_id);
        }

        // Multi-chunk: allocate the aggregate id up front so each confirmed chunk
        // reference is persisted under a stable id as it is sent, and a
        // mid-message failure compensates by deleting the already-sent chunks
        // instead of orphaning visible messages.
        let message_id = MessageId::new(Uuid::now_v7().to_string());
        let sent_refs =
            send_multi_chunk_tracked(self, &self.outbound_refs, &target, &message_id, &rendered)
                .await?;
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
        let msg = prepare_outbound_message(msg);

        let existing = self.outbound_refs.resolve(msg_id).await?;
        let rendered = self.renderer.render(&msg);
        apply_edit_tracked(self, &self.outbound_refs, msg_id, &existing, &rendered).await?;
        Ok(())
    }

    async fn delete_locked(&self, msg_id: &MessageId) -> Result<()> {
        let refs = self.outbound_refs.resolve(msg_id).await?;
        for message_ref in &refs {
            self.delete_ref(message_ref).await?;
        }
        self.outbound_refs
            .remove_after_external_side_effect(msg_id, "chat.delete")
            .await;
        Ok(())
    }
}

#[async_trait]
impl SlackChunkTransport for SlackAdapter {
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

    async fn delete_ref(&self, message_ref: &SlackMessageRef) -> Result<()> {
        self.rate_limiter
            .wait_for_channel_slot(message_ref.channel_id.as_ref())
            .await?;
        let session = self.client.open_session(&self.bot_token);
        let request = SlackApiChatDeleteRequest {
            channel: SlackChannelId(message_ref.channel_id.to_string()),
            ts: SlackTs(message_ref.ts.clone()),
            as_user: None,
        };

        async {
            match session.chat_delete(&request).await {
                Ok(_) => Ok(()),
                Err(SlackClientError::ApiError(api)) if api.code == "message_not_found" => {
                    tracing::debug!(
                        slack.channel_id = %message_ref.channel_id,
                        slack.ts = %message_ref.ts,
                        "Slack delete skipped a chunk that was already absent"
                    );
                    Ok(())
                }
                Err(error) => Err(slack_client_error("chat.delete", error)),
            }
        }
        .instrument(slack_api_span("slack_message_delete", "chat.delete"))
        .await
    }
}

fn slack_message_content(chunk: &SlackRenderChunk) -> SlackMessageContent {
    SlackMessageContent {
        text: Some(chunk.text.clone()),
        blocks: None,
        attachments: None,
        upload: None,
        files: None,
        reactions: None,
        metadata: None,
    }
}
