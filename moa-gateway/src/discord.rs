//! Discord platform adapter built on top of `serenity`.

use std::{collections::HashMap, env, sync::Arc, time::Instant};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    Attachment, ChannelRef, InboundMessage, MessageId, MoaConfig, MoaError, OutboundMessage,
    Platform, PlatformAdapter, PlatformCapabilities, PlatformUser, Result,
};
use serenity::all::{
    AutoArchiveDuration, ButtonStyle as DiscordButtonStyle, Channel, ChannelId, ChannelType,
    Client, ComponentInteraction, Context, CreateActionRow, CreateButton, CreateEmbed,
    CreateMessage, CreateThread, EditMessage, EventHandler, GatewayIntents, Http,
    Interaction as DiscordInteraction, Message as DiscordMessage, MessageId as DiscordMessageId,
    User as DiscordUser, UserId as DiscordUserId,
};
use tokio::{
    sync::{Mutex, mpsc},
    time::sleep,
};
use tracing::Instrument;
use tracing::warn;
use uuid::Uuid;

use crate::{
    approval::{ApprovalCallbackAction, prepare_outbound_message},
    gateway_receive_span,
    renderer::{DiscordRenderChunk, DiscordRenderer},
};

#[derive(Clone)]
struct DiscordEventHandlerState {
    event_tx: mpsc::Sender<InboundMessage>,
    inbound_contexts: Arc<Mutex<HashMap<String, DiscordInboundContext>>>,
}

/// Discord adapter implementing the generic platform abstraction.
#[derive(Clone)]
pub struct DiscordAdapter {
    token: Arc<String>,
    http: Arc<Http>,
    renderer: DiscordRenderer,
    inbound_contexts: Arc<Mutex<HashMap<String, DiscordInboundContext>>>,
    outbound_messages: Arc<Mutex<HashMap<String, Vec<DiscordMessageRef>>>>,
    last_edits: Arc<Mutex<HashMap<String, Instant>>>,
}

impl DiscordAdapter {
    /// Creates a Discord adapter from a bot token.
    pub fn new(token: impl Into<String>) -> Self {
        let token = token.into();
        Self {
            http: Arc::new(Http::new(&token)),
            token: Arc::new(token),
            renderer: DiscordRenderer::new(),
            inbound_contexts: Arc::new(Mutex::new(HashMap::new())),
            outbound_messages: Arc::new(Mutex::new(HashMap::new())),
            last_edits: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Creates a Discord adapter using the configured token environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let env_name = &config.gateway.discord_token_env;
        let token = env::var(env_name)
            .map_err(|_| MoaError::MissingEnvironmentVariable(env_name.clone()))?;
        Ok(Self::new(token))
    }

    async fn resolve_target(&self, reply_to: Option<&str>) -> Result<DiscordTarget> {
        let reply_to = reply_to.ok_or_else(|| {
            MoaError::ValidationError("discord outbound messages require reply_to context".into())
        })?;

        if let Some(last_ref) = self
            .outbound_messages
            .lock()
            .await
            .get(reply_to)
            .and_then(|refs| refs.last().copied())
        {
            return Ok(DiscordTarget::Channel(last_ref.channel_id));
        }

        if let Some(inbound_ref) = self.inbound_contexts.lock().await.get(reply_to).cloned() {
            if inbound_ref.is_direct {
                return Ok(DiscordTarget::Channel(inbound_ref.channel_id));
            }

            if let Some(thread_id) = inbound_ref.thread_id {
                return Ok(DiscordTarget::Channel(thread_id));
            }

            return Ok(DiscordTarget::CreateThread {
                context_key: reply_to.to_string(),
                channel_id: inbound_ref.channel_id,
                message_id: inbound_ref.message_id,
            });
        }

        Err(MoaError::ValidationError(format!(
            "discord reply target not found: {reply_to}"
        )))
    }

    async fn wait_for_edit_window(&self, message_id: &MessageId) {
        let min_interval = self.capabilities().min_edit_interval;
        let sleep_for = {
            let last_edits = self.last_edits.lock().await;
            last_edits
                .get(message_id.as_str())
                .copied()
                .map(|last_edit| min_interval.saturating_sub(last_edit.elapsed()))
        };
        if let Some(delay) = sleep_for.filter(|delay| !delay.is_zero()) {
            sleep(delay).await;
        }
        self.last_edits
            .lock()
            .await
            .insert(message_id.as_str().to_string(), Instant::now());
    }

    async fn ensure_target_channel(&self, target: DiscordTarget) -> Result<ChannelId> {
        match target {
            DiscordTarget::Channel(channel_id) => Ok(channel_id),
            DiscordTarget::CreateThread {
                context_key,
                channel_id,
                message_id,
            } => {
                let thread = channel_id
                    .create_thread_from_message(
                        &self.http,
                        message_id,
                        CreateThread::new(format!("moa-{}", message_id.get()))
                            .auto_archive_duration(AutoArchiveDuration::OneDay),
                    )
                    .await
                    .map_err(|error| MoaError::ProviderError(error.to_string()))?;
                let thread_id = thread.id;
                if let Some(context) = self.inbound_contexts.lock().await.get_mut(&context_key) {
                    context.thread_id = Some(thread_id);
                }
                Ok(thread_id)
            }
        }
    }

    async fn send_chunk(
        &self,
        channel_id: ChannelId,
        chunk: &DiscordRenderChunk,
    ) -> Result<DiscordMessageRef> {
        let builder = discord_create_message(chunk);
        let message = channel_id
            .send_message(&self.http, builder)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(DiscordMessageRef {
            channel_id,
            message_id: message.id,
        })
    }

    async fn update_chunk(
        &self,
        message_ref: DiscordMessageRef,
        chunk: &DiscordRenderChunk,
    ) -> Result<()> {
        let builder = discord_edit_message(chunk);
        message_ref
            .channel_id
            .edit_message(&self.http, message_ref.message_id, builder)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    /// Returns the adapter platform identifier.
    fn platform(&self) -> Platform {
        self.renderer.platform()
    }

    /// Returns Discord transport capabilities.
    fn capabilities(&self) -> PlatformCapabilities {
        self.renderer.capabilities()
    }

    /// Starts the Discord gateway client and forwards normalized updates.
    async fn start(&self, event_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;
        let handler = DiscordGatewayHandler {
            shared: DiscordEventHandlerState {
                event_tx,
                inbound_contexts: self.inbound_contexts.clone(),
            },
        };
        let mut client = Client::builder(self.token.as_ref().as_str(), intents)
            .event_handler(handler)
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        client
            .start()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(())
    }

    /// Sends a new outbound Discord message, splitting across embed-sized chunks.
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        let msg = prepare_outbound_message(self.platform(), &self.capabilities(), msg);
        let target = self.resolve_target(msg.reply_to.as_deref()).await?;
        let channel_id = self.ensure_target_channel(target).await?;
        let rendered = self.renderer.render(&msg);
        let synthetic_id = MessageId::new(Uuid::new_v4().to_string());
        let mut sent_refs = Vec::with_capacity(rendered.len());
        for chunk in &rendered {
            let sent_ref = self.send_chunk(channel_id, chunk).await?;
            sent_refs.push(sent_ref);
        }
        self.outbound_messages
            .lock()
            .await
            .insert(synthetic_id.as_str().to_string(), sent_refs);
        Ok(synthetic_id)
    }

    /// Edits an existing outbound Discord message in place.
    async fn edit(&self, msg_id: &MessageId, msg: OutboundMessage) -> Result<()> {
        self.wait_for_edit_window(msg_id).await;
        let msg = prepare_outbound_message(self.platform(), &self.capabilities(), msg);
        let existing = self
            .outbound_messages
            .lock()
            .await
            .get(msg_id.as_str())
            .cloned()
            .ok_or_else(|| {
                MoaError::ValidationError(format!("unknown discord message id: {msg_id}"))
            })?;
        let rendered = self.renderer.render(&msg);
        let overlap = existing.len().min(rendered.len());
        let mut updated_refs = Vec::with_capacity(rendered.len());

        for index in 0..overlap {
            let message_ref = existing[index];
            self.update_chunk(message_ref, &rendered[index]).await?;
            updated_refs.push(message_ref);
        }

        if rendered.len() > existing.len() {
            let channel_id = existing
                .last()
                .map(|message_ref| message_ref.channel_id)
                .ok_or_else(|| {
                    MoaError::ValidationError(format!("discord message id {msg_id} has no refs"))
                })?;
            for chunk in rendered.iter().skip(existing.len()) {
                let sent_ref = self.send_chunk(channel_id, chunk).await?;
                updated_refs.push(sent_ref);
            }
        }

        if existing.len() > rendered.len() {
            for message_ref in existing.iter().skip(rendered.len()) {
                message_ref
                    .channel_id
                    .delete_message(&self.http, message_ref.message_id)
                    .await
                    .map_err(|error| MoaError::ProviderError(error.to_string()))?;
            }
        }

        self.outbound_messages
            .lock()
            .await
            .insert(msg_id.as_str().to_string(), updated_refs);
        Ok(())
    }

    /// Deletes a Discord message sent through this adapter.
    async fn delete(&self, msg_id: &MessageId) -> Result<()> {
        let refs = self
            .outbound_messages
            .lock()
            .await
            .remove(msg_id.as_str())
            .ok_or_else(|| {
                MoaError::ValidationError(format!("unknown discord message id: {msg_id}"))
            })?;
        for message_ref in refs {
            message_ref
                .channel_id
                .delete_message(&self.http, message_ref.message_id)
                .await
                .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct DiscordGatewayHandler {
    shared: DiscordEventHandlerState,
}

#[serenity::async_trait]
impl EventHandler for DiscordGatewayHandler {
    async fn message(&self, ctx: Context, new_message: DiscordMessage) {
        if new_message.author.bot {
            return;
        }

        if let Some((inbound, context_ref)) = inbound_from_message(&ctx, &new_message).await {
            let gateway_span = gateway_receive_span(&inbound);
            async {
                self.shared
                    .inbound_contexts
                    .lock()
                    .await
                    .insert(inbound.platform_msg_id.clone(), context_ref);
                if self.shared.event_tx.send(inbound).await.is_err() {
                    warn!("discord inbound receiver dropped");
                }
            }
            .instrument(gateway_span)
            .await;
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: DiscordInteraction) {
        let Some(component) = interaction.message_component() else {
            return;
        };

        if let Err(error) = component.defer(&ctx.http).await {
            warn!("failed to defer discord component interaction: {error}");
        }

        if let Some((inbound, context_ref)) = inbound_from_component_interaction(&component) {
            let gateway_span = gateway_receive_span(&inbound);
            async {
                self.shared
                    .inbound_contexts
                    .lock()
                    .await
                    .insert(inbound.platform_msg_id.clone(), context_ref);
                if self.shared.event_tx.send(inbound).await.is_err() {
                    warn!("discord inbound receiver dropped");
                }
            }
            .instrument(gateway_span)
            .await;
        }
    }
}

#[derive(Debug, Clone)]
struct DiscordInboundContext {
    channel_id: ChannelId,
    message_id: DiscordMessageId,
    thread_id: Option<ChannelId>,
    parent_channel_id: Option<ChannelId>,
    is_direct: bool,
}

impl DiscordInboundContext {
    fn channel_ref(&self, user_id: DiscordUserId) -> ChannelRef {
        if self.is_direct {
            return ChannelRef::DirectMessage {
                user_id: user_id.get().to_string(),
            };
        }

        if let Some(thread_id) = self.thread_id {
            return ChannelRef::Thread {
                channel_id: self
                    .parent_channel_id
                    .unwrap_or(self.channel_id)
                    .get()
                    .to_string(),
                thread_id: thread_id.get().to_string(),
            };
        }

        ChannelRef::Group {
            channel_id: self.channel_id.get().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DiscordMessageRef {
    channel_id: ChannelId,
    message_id: DiscordMessageId,
}

enum DiscordTarget {
    Channel(ChannelId),
    CreateThread {
        context_key: String,
        channel_id: ChannelId,
        message_id: DiscordMessageId,
    },
}

async fn inbound_from_message(
    ctx: &Context,
    message: &DiscordMessage,
) -> Option<(InboundMessage, DiscordInboundContext)> {
    let text = if message.content.is_empty() {
        return None;
    } else {
        message.content.clone()
    };

    let context_ref = match message.channel_id.to_channel(&ctx.http).await.ok()? {
        Channel::Private(_) => DiscordInboundContext {
            channel_id: message.channel_id,
            message_id: message.id,
            thread_id: None,
            parent_channel_id: None,
            is_direct: true,
        },
        Channel::Guild(channel) => {
            if is_thread_kind(channel.kind) {
                DiscordInboundContext {
                    channel_id: channel.parent_id.unwrap_or(channel.id),
                    message_id: message.id,
                    thread_id: Some(channel.id),
                    parent_channel_id: channel.parent_id,
                    is_direct: false,
                }
            } else {
                DiscordInboundContext {
                    channel_id: channel.id,
                    message_id: message.id,
                    thread_id: message.thread.as_ref().map(|thread| thread.id),
                    parent_channel_id: None,
                    is_direct: false,
                }
            }
        }
        _ => DiscordInboundContext {
            channel_id: message.channel_id,
            message_id: message.id,
            thread_id: None,
            parent_channel_id: None,
            is_direct: false,
        },
    };

    let inbound = InboundMessage {
        platform: Platform::Discord,
        platform_msg_id: message.id.get().to_string(),
        user: PlatformUser {
            platform_id: message.author.id.get().to_string(),
            display_name: discord_user_name(&message.author),
            moa_user_id: None,
        },
        channel: context_ref.channel_ref(message.author.id),
        text,
        attachments: attachments_from_message(message),
        reply_to: message
            .referenced_message
            .as_ref()
            .map(|reference| reference.id.get().to_string()),
        timestamp: chrono::DateTime::<Utc>::from_timestamp(message.timestamp.unix_timestamp(), 0)
            .unwrap_or_else(Utc::now),
    };

    Some((inbound, context_ref))
}

fn inbound_from_component_interaction(
    interaction: &ComponentInteraction,
) -> Option<(InboundMessage, DiscordInboundContext)> {
    let action = ApprovalCallbackAction::decode(&interaction.data.custom_id)?;
    let context_ref = context_from_component(interaction);
    let platform_msg_id = format!(
        "callback:{}:{}",
        interaction.message.id.get(),
        interaction.id.get()
    );
    let inbound = InboundMessage {
        platform: Platform::Discord,
        platform_msg_id,
        user: PlatformUser {
            platform_id: interaction.user.id.get().to_string(),
            display_name: discord_user_name(&interaction.user),
            moa_user_id: None,
        },
        channel: context_ref.channel_ref(interaction.user.id),
        text: action.inbound_command(),
        attachments: Vec::new(),
        reply_to: Some(interaction.message.id.get().to_string()),
        timestamp: Utc::now(),
    };
    Some((inbound, context_ref))
}

fn context_from_component(interaction: &ComponentInteraction) -> DiscordInboundContext {
    let channel = interaction.channel.as_ref();
    let is_direct = channel.is_some_and(|channel| channel.kind == ChannelType::Private);
    let thread_id = channel
        .filter(|channel| is_thread_kind(channel.kind))
        .map(|channel| channel.id);
    let parent_channel_id = channel.and_then(|channel| channel.parent_id);
    let channel_id = if let Some(thread_id) = thread_id {
        parent_channel_id.unwrap_or(thread_id)
    } else {
        interaction.channel_id
    };

    DiscordInboundContext {
        channel_id,
        message_id: interaction.message.id,
        thread_id,
        parent_channel_id,
        is_direct,
    }
}

fn attachments_from_message(message: &DiscordMessage) -> Vec<Attachment> {
    message
        .attachments
        .iter()
        .map(|attachment| Attachment {
            name: attachment.filename.clone(),
            mime_type: attachment.content_type.clone(),
            url: Some(attachment.url.clone()),
            path: None,
            size_bytes: Some(u64::from(attachment.size)),
        })
        .collect()
}

fn discord_user_name(user: &DiscordUser) -> String {
    user.global_name
        .clone()
        .unwrap_or_else(|| user.name.clone())
}

fn is_thread_kind(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    )
}

fn discord_create_message(chunk: &DiscordRenderChunk) -> CreateMessage {
    let mut builder = CreateMessage::new();
    if let Some(content) = chunk.content.as_ref() {
        builder = builder.content(content.clone());
    }
    if let Some(embed) = discord_embed(chunk) {
        builder = builder.embed(embed);
    }
    if !chunk.buttons.is_empty() {
        builder = builder.components(vec![CreateActionRow::Buttons(
            chunk.buttons.iter().map(discord_button).collect(),
        )]);
    }
    builder
}

fn discord_edit_message(chunk: &DiscordRenderChunk) -> EditMessage {
    let mut builder = EditMessage::new().content(chunk.content.clone().unwrap_or_default());
    builder = match discord_embed(chunk) {
        Some(embed) => builder.embeds(vec![embed]),
        None => builder.embeds(Vec::new()),
    };
    builder = if chunk.buttons.is_empty() {
        builder.components(Vec::new())
    } else {
        builder.components(vec![CreateActionRow::Buttons(
            chunk.buttons.iter().map(discord_button).collect(),
        )])
    };
    builder
}

fn discord_embed(chunk: &DiscordRenderChunk) -> Option<CreateEmbed> {
    let description = chunk.embed_description.as_ref()?;
    let mut embed = CreateEmbed::new().description(description.clone());
    if let Some(title) = chunk.embed_title.as_ref() {
        embed = embed.title(title.clone());
    }
    if let Some(color) = chunk.embed_color {
        embed = embed.color(color);
    }
    Some(embed)
}

fn discord_button(button: &moa_core::ActionButton) -> CreateButton {
    CreateButton::new(button.callback_data.clone())
        .label(button.label.clone())
        .style(match button.style {
            moa_core::ButtonStyle::Primary => DiscordButtonStyle::Primary,
            moa_core::ButtonStyle::Danger => DiscordButtonStyle::Danger,
            moa_core::ButtonStyle::Secondary => DiscordButtonStyle::Secondary,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_callback_maps_to_control_message() {
        let request_id = Uuid::new_v4();
        let interaction: ComponentInteraction = serde_json::from_value(json!({
            "id": "100",
            "application_id": "200",
            "type": 3,
            "data": {
                "component_type": 2,
                "custom_id": ApprovalCallbackAction::AlwaysAllow { request_id }.encode()
            },
            "guild_id": "300",
            "channel": {
                "id": "400",
                "type": 11,
                "name": "moa-123",
                "parent_id": "401"
            },
            "channel_id": "400",
            "member": {
                "user": {
                    "id": "500",
                    "username": "alice",
                    "discriminator": "0001",
                    "global_name": "Alice",
                    "avatar": null,
                    "bot": false
                },
                "roles": [],
                "joined_at": "2026-04-09T12:00:00.000000+00:00",
                "deaf": false,
                "mute": false,
                "flags": 0,
                "pending": false,
                "permissions": "0"
            },
            "user": {
                "id": "500",
                "username": "alice",
                "discriminator": "0001",
                "global_name": "Alice",
                "avatar": null,
                "bot": false
            },
            "token": "token",
            "version": 1,
            "message": {
                "id": "600",
                "channel_id": "400",
                "author": {
                    "id": "700",
                    "username": "moa-bot",
                    "discriminator": "0001",
                    "global_name": "MOA",
                    "avatar": null,
                    "bot": true
                },
                "content": "approval",
                "timestamp": "2026-04-09T12:00:00.000000+00:00",
                "edited_timestamp": null,
                "tts": false,
                "mention_everyone": false,
                "mentions": [],
                "mention_roles": [],
                "attachments": [],
                "embeds": [],
                "reactions": [],
                "pinned": false,
                "type": 0,
                "flags": 0,
                "components": []
            },
            "locale": "en-US",
            "entitlements": [],
            "authorizing_integration_owners": {},
            "attachment_size_limit": 10485760
        }))
        .expect("discord component interaction should deserialize");

        let inbound =
            inbound_from_component_interaction(&interaction).expect("normalized component");
        assert_eq!(inbound.0.platform, Platform::Discord);
        assert_eq!(inbound.0.text, format!("/approval always {request_id}"));
        assert_eq!(inbound.0.reply_to, Some("600".to_string()));
        assert_eq!(
            inbound.0.channel,
            ChannelRef::Thread {
                channel_id: "401".to_string(),
                thread_id: "400".to_string(),
            }
        );
    }

    #[test]
    fn discord_create_message_includes_buttons_for_last_chunk() {
        let chunk = DiscordRenderChunk {
            content: None,
            embed_title: Some("Approval required".to_string()),
            embed_description: Some("Review this file write.".to_string()),
            embed_color: Some(0xF59E0B),
            buttons: crate::approval::approval_buttons(Platform::Discord, Uuid::new_v4()),
        };

        let builder = discord_create_message(&chunk);
        let value =
            serde_json::to_value(builder).expect("discord message builder should serialize");
        assert!(value.get("components").is_some());
        assert!(value.get("embeds").is_some());
    }
}
