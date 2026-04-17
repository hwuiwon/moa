//! Slack platform adapter built on top of `slack-morphism` Socket Mode.

use std::{collections::HashMap, env, sync::Arc, time::Instant};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    ChannelRef, InboundMessage, MessageId, MoaConfig, MoaError, OutboundMessage, Platform,
    PlatformAdapter, PlatformCapabilities, PlatformUser, Result,
};
use slack_morphism::prelude::*;
use tokio::{
    sync::{RwLock, mpsc},
    time::sleep,
};
use tracing::Instrument;
use tracing::warn;
use uuid::Uuid;

use crate::{
    approval::{ApprovalCallbackAction, prepare_outbound_message},
    gateway_receive_span,
    renderer::{SlackRenderChunk, SlackRenderer},
};

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
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
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
        let bot_env = &config.gateway.slack_token_env;
        let app_env = &config.gateway.slack_app_token_env;
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
            channel: SlackChannelId(target.channel_id.clone()),
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

        let response = session
            .chat_post_message(&request)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(SlackMessageRef {
            channel_id: response.channel.0,
            ts: response.ts.0,
            thread_ts: Some(target.thread_ts.clone()),
        })
    }

    async fn update_chunk(
        &self,
        message_ref: &SlackMessageRef,
        chunk: &SlackRenderChunk,
    ) -> Result<()> {
        let session = self.client.open_session(&self.bot_token);
        let request = SlackApiChatUpdateRequest {
            channel: SlackChannelId(message_ref.channel_id.clone()),
            content: slack_message_content(chunk),
            ts: SlackTs(message_ref.ts.clone()),
            as_user: None,
            link_names: None,
            parse: None,
            reply_broadcast: None,
        };

        session
            .chat_update(&request)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(())
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
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
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
                    channel: SlackChannelId(message_ref.channel_id.clone()),
                    ts: SlackTs(message_ref.ts.clone()),
                    as_user: None,
                };
                session
                    .chat_delete(&request)
                    .await
                    .map_err(|error| MoaError::ProviderError(error.to_string()))?;
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
                channel: SlackChannelId(message_ref.channel_id),
                ts: SlackTs(message_ref.ts),
                as_user: None,
            };
            session
                .chat_delete(&request)
                .await
                .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        }
        Ok(())
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
        let gateway_span = gateway_receive_span(&inbound);
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
        .instrument(gateway_span)
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
        let gateway_span = gateway_receive_span(&inbound);
        async {
            if shared.event_tx.send(inbound).await.is_err() {
                warn!("slack inbound receiver dropped");
            }
        }
        .instrument(gateway_span)
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
            channel_id: message.channel.0.clone(),
            ts: message.origin.ts.0.clone(),
            thread_ts: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        }),
        SlackEventCallbackBody::Message(message) => Some(SlackMessageRef {
            channel_id: message.origin.channel.as_ref()?.0.clone(),
            ts: message.origin.ts.0.clone(),
            thread_ts: message.origin.thread_ts.as_ref().map(|ts| ts.0.clone()),
        }),
        _ => None,
    }
}

async fn inbound_from_interaction_event(
    event: &SlackInteractionEvent,
    inbound_contexts: Arc<RwLock<HashMap<String, SlackMessageRef>>>,
) -> Option<InboundMessage> {
    let block_actions = match event {
        SlackInteractionEvent::BlockActions(block_actions) => block_actions,
        _ => return None,
    };

    let action = block_actions.actions.as_ref()?.first()?;
    let callback = ApprovalCallbackAction::decode(action.value.as_deref()?)?;
    let user = block_actions.user.as_ref()?;
    let origin = interaction_origin(block_actions)?;
    let platform_msg_id = format!(
        "callback:{}:{}",
        origin.ts,
        action
            .action_ts
            .as_ref()
            .map(|ts| ts.0.clone())
            .unwrap_or_else(|| Uuid::now_v7().to_string())
    );

    inbound_contexts
        .write()
        .await
        .insert(platform_msg_id.clone(), origin.clone());
    Some(InboundMessage {
        platform: Platform::Slack,
        platform_msg_id,
        user: PlatformUser {
            platform_id: user.id.0.clone(),
            display_name: slack_basic_user_name(user),
            moa_user_id: None,
        },
        channel: slack_channel_ref(&origin.channel_id, Some(origin.thread_anchor()), &user.id.0),
        text: callback.inbound_command(),
        attachments: Vec::new(),
        reply_to: Some(origin.ts),
        timestamp: Utc::now(),
    })
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

fn interaction_origin(event: &SlackInteractionBlockActionsEvent) -> Option<SlackMessageRef> {
    let (message_ts, channel_id) = match &event.container {
        SlackInteractionActionContainer::Message(container) => (
            container.message_ts.0.clone(),
            container.channel_id.as_ref()?.0.clone(),
        ),
        SlackInteractionActionContainer::MessageAttachment(container) => (
            container.message_ts.0.clone(),
            container.channel_id.as_ref()?.0.clone(),
        ),
        SlackInteractionActionContainer::View(_) => return None,
    };

    let thread_ts = event
        .message
        .as_ref()
        .and_then(|message| message.origin.thread_ts.as_ref())
        .map(|ts| ts.0.clone());

    Some(SlackMessageRef {
        channel_id,
        ts: message_ts,
        thread_ts,
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

fn slack_basic_user_name(user: &SlackBasicUserInfo) -> String {
    user.name
        .clone()
        .or_else(|| user.username.clone())
        .unwrap_or_else(|| format!("<@{}>", user.id.0))
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
    async fn parses_approval_callback_into_control_message() {
        let request_id = Uuid::now_v7();
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
                "text": "approval",
                "ts": "1712668800.000200",
                "thread_ts": "1712668800.000050",
                "channel": "C123"
            },
            "actions": [{
                "type": "button",
                "action_id": "allow",
                "value": ApprovalCallbackAction::AlwaysAllow { request_id }.encode()
            }]
        }))
        .expect("slack interaction should deserialize");

        let inbound = inbound_from_interaction_event(&event, Arc::new(Mutex::new(HashMap::new())))
            .await
            .expect("normalized callback");

        assert_eq!(inbound.platform, Platform::Slack);
        assert_eq!(inbound.text, format!("/approval always {request_id}"));
        assert_eq!(inbound.reply_to, Some("1712668800.000200".to_string()));
        assert_eq!(
            inbound.channel,
            ChannelRef::Thread {
                channel_id: "C123".to_string(),
                thread_id: "1712668800.000050".to_string()
            }
        );
    }
}
