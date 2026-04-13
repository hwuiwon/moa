//! Telegram platform adapter built on top of `teloxide`.

use std::{collections::HashMap, env, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    ChannelRef, InboundMessage, MessageId, MoaConfig, MoaError, OutboundMessage, Platform,
    PlatformAdapter, PlatformCapabilities, PlatformUser, Result,
};
use teloxide::{
    dptree,
    payloads::SendMessageSetters,
    prelude::*,
    sugar::request::RequestReplyExt,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, Message, Update, User},
};
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument;
use tracing::warn;
use uuid::Uuid;

use crate::{
    approval::{ApprovalCallbackAction, prepare_outbound_message},
    gateway_receive_span,
    renderer::{TelegramRenderChunk, TelegramRenderer},
};

/// Telegram adapter implementing the generic platform abstraction.
#[derive(Clone)]
pub struct TelegramAdapter {
    bot: Bot,
    renderer: TelegramRenderer,
    inbound_contexts: Arc<Mutex<HashMap<String, TelegramMessageRef>>>,
    outbound_messages: Arc<Mutex<HashMap<String, Vec<TelegramMessageRef>>>>,
}

impl TelegramAdapter {
    /// Creates a Telegram adapter from a bot token.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            bot: Bot::new(token.into()),
            renderer: TelegramRenderer::new(),
            inbound_contexts: Arc::new(Mutex::new(HashMap::new())),
            outbound_messages: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Creates a Telegram adapter using the configured token environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let env_name = &config.gateway.telegram_token_env;
        let token = env::var(env_name)
            .map_err(|_| MoaError::MissingEnvironmentVariable(env_name.clone()))?;
        Ok(Self::new(token))
    }

    async fn resolve_target(&self, reply_to: Option<&str>) -> Result<TelegramTarget> {
        let reply_to = reply_to.ok_or_else(|| {
            MoaError::ValidationError(
                "telegram outbound messages require reply_to context".to_string(),
            )
        })?;

        if let Some(last_ref) = self
            .outbound_messages
            .lock()
            .await
            .get(reply_to)
            .and_then(|refs| refs.last().copied())
        {
            return Ok(TelegramTarget {
                chat_id: last_ref.chat_id,
                reply_to_message_id: Some(last_ref.message_id),
            });
        }

        let message_id = parse_message_id(reply_to)?;
        if let Some(inbound_ref) = self.inbound_contexts.lock().await.get(reply_to).copied() {
            return Ok(TelegramTarget {
                chat_id: inbound_ref.chat_id,
                reply_to_message_id: Some(message_id),
            });
        }

        Err(MoaError::ValidationError(format!(
            "telegram reply target not found: {reply_to}"
        )))
    }

    async fn send_chunk(
        &self,
        chat_id: ChatId,
        reply_to: Option<teloxide::types::MessageId>,
        chunk: &TelegramRenderChunk,
    ) -> Result<TelegramMessageRef> {
        let mut request = self.bot.send_message(chat_id, chunk.text.clone());
        if let Some(reply_to) = reply_to {
            request = request.reply_to(reply_to);
        }
        let markup = inline_keyboard(&chunk.buttons);
        if let Some(markup) = markup {
            request = request.reply_markup(markup);
        }

        let message = request
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(TelegramMessageRef::from_message(&message))
    }

    async fn update_chunk(
        &self,
        message_ref: TelegramMessageRef,
        chunk: &TelegramRenderChunk,
    ) -> Result<()> {
        let mut request = self.bot.edit_message_text(
            ChatId(message_ref.chat_id),
            teloxide::types::MessageId(message_ref.message_id),
            chunk.text.clone(),
        );
        if let Some(markup) = inline_keyboard(&chunk.buttons) {
            request = request.reply_markup(markup);
        }

        request
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(())
    }

    async fn clear_buttons(&self, message_ref: TelegramMessageRef) -> Result<()> {
        self.bot
            .edit_message_reply_markup(
                ChatId(message_ref.chat_id),
                teloxide::types::MessageId(message_ref.message_id),
            )
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    /// Returns the adapter platform identifier.
    fn platform(&self) -> Platform {
        self.renderer.platform()
    }

    /// Returns Telegram transport capabilities.
    fn capabilities(&self) -> PlatformCapabilities {
        self.renderer.capabilities()
    }

    /// Starts the Telegram polling loop and forwards normalized updates.
    async fn start(&self, event_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let handler = dptree::entry()
            .branch(Update::filter_message().endpoint(handle_message))
            .branch(Update::filter_callback_query().endpoint(handle_callback_query));

        Dispatcher::builder(self.bot.clone(), handler)
            .dependencies(dptree::deps![
                event_tx,
                self.inbound_contexts.clone(),
                self.outbound_messages.clone()
            ])
            .build()
            .dispatch()
            .await;

        Ok(())
    }

    /// Sends a new outbound Telegram message, splitting at Telegram's length limit.
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        let msg = prepare_outbound_message(self.platform(), &self.capabilities(), msg);
        let target = self.resolve_target(msg.reply_to.as_deref()).await?;
        let rendered = self.renderer.render(&msg);
        let mut sent_refs = Vec::with_capacity(rendered.len());
        let mut reply_to = target.reply_to_message_id.map(teloxide::types::MessageId);

        for chunk in &rendered {
            let sent_ref = self
                .send_chunk(ChatId(target.chat_id), reply_to, chunk)
                .await?;
            reply_to = Some(teloxide::types::MessageId(sent_ref.message_id));
            sent_refs.push(sent_ref);
        }

        let synthetic_id = MessageId::new(Uuid::now_v7().to_string());
        self.outbound_messages
            .lock()
            .await
            .insert(synthetic_id.as_str().to_string(), sent_refs);
        Ok(synthetic_id)
    }

    /// Edits an existing outbound Telegram message.
    async fn edit(&self, msg_id: &MessageId, msg: OutboundMessage) -> Result<()> {
        let msg = prepare_outbound_message(self.platform(), &self.capabilities(), msg);
        let existing = self
            .outbound_messages
            .lock()
            .await
            .get(msg_id.as_str())
            .cloned()
            .ok_or_else(|| {
                MoaError::ValidationError(format!("unknown telegram message id: {msg_id}"))
            })?;
        let rendered = self.renderer.render(&msg);
        let mut updated_refs = Vec::with_capacity(rendered.len());
        let overlap = existing.len().min(rendered.len());

        for (message_ref, chunk) in existing.iter().copied().zip(rendered.iter()).take(overlap) {
            self.update_chunk(message_ref, chunk).await?;
            updated_refs.push(message_ref);
        }

        if rendered.len() > existing.len() {
            let chat_id = existing
                .first()
                .map(|message_ref| message_ref.chat_id)
                .ok_or_else(|| {
                    MoaError::ValidationError(format!("telegram message id {msg_id} has no refs"))
                })?;
            let mut reply_to = updated_refs
                .last()
                .copied()
                .or_else(|| existing.last().copied())
                .map(|message_ref| teloxide::types::MessageId(message_ref.message_id));
            for chunk in rendered.iter().skip(existing.len()) {
                let sent_ref = self.send_chunk(ChatId(chat_id), reply_to, chunk).await?;
                reply_to = Some(teloxide::types::MessageId(sent_ref.message_id));
                updated_refs.push(sent_ref);
            }
        }

        if existing.len() > rendered.len() {
            for message_ref in existing.iter().copied().skip(rendered.len()) {
                self.bot
                    .delete_message(
                        ChatId(message_ref.chat_id),
                        teloxide::types::MessageId(message_ref.message_id),
                    )
                    .await
                    .map_err(|error| MoaError::ProviderError(error.to_string()))?;
            }
        }

        if rendered.len() > 1 {
            for message_ref in updated_refs
                .iter()
                .copied()
                .take(updated_refs.len().saturating_sub(1))
            {
                self.clear_buttons(message_ref).await?;
            }
        }

        self.outbound_messages
            .lock()
            .await
            .insert(msg_id.as_str().to_string(), updated_refs);
        Ok(())
    }

    /// Deletes a Telegram message sent through this adapter.
    async fn delete(&self, msg_id: &MessageId) -> Result<()> {
        let refs = self
            .outbound_messages
            .lock()
            .await
            .remove(msg_id.as_str())
            .ok_or_else(|| {
                MoaError::ValidationError(format!("unknown telegram message id: {msg_id}"))
            })?;

        for message_ref in refs {
            self.bot
                .delete_message(
                    ChatId(message_ref.chat_id),
                    teloxide::types::MessageId(message_ref.message_id),
                )
                .await
                .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct TelegramTarget {
    chat_id: i64,
    reply_to_message_id: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
struct TelegramMessageRef {
    chat_id: i64,
    message_id: i32,
}

impl TelegramMessageRef {
    fn from_message(message: &Message) -> Self {
        Self {
            chat_id: message.chat.id.0,
            message_id: message.id.0,
        }
    }
}

async fn handle_message(
    msg: Message,
    event_tx: mpsc::Sender<InboundMessage>,
    inbound_contexts: Arc<Mutex<HashMap<String, TelegramMessageRef>>>,
) -> std::result::Result<(), teloxide::RequestError> {
    if let Some(inbound) = inbound_from_message(&msg) {
        let gateway_span = gateway_receive_span(&inbound);
        async {
            inbound_contexts.lock().await.insert(
                inbound.platform_msg_id.clone(),
                TelegramMessageRef::from_message(&msg),
            );
            if event_tx.send(inbound).await.is_err() {
                warn!("telegram inbound receiver dropped");
            }
        }
        .instrument(gateway_span)
        .await;
    }

    Ok(())
}

async fn handle_callback_query(
    bot: Bot,
    query: CallbackQuery,
    event_tx: mpsc::Sender<InboundMessage>,
    inbound_contexts: Arc<Mutex<HashMap<String, TelegramMessageRef>>>,
    outbound_messages: Arc<Mutex<HashMap<String, Vec<TelegramMessageRef>>>>,
) -> std::result::Result<(), teloxide::RequestError> {
    bot.answer_callback_query(query.id.clone()).await?;
    if let Some(inbound) =
        inbound_from_callback_query(&query, inbound_contexts.clone(), outbound_messages.clone())
            .await
    {
        let gateway_span = gateway_receive_span(&inbound);
        async {
            if event_tx.send(inbound).await.is_err() {
                warn!("telegram inbound receiver dropped");
            }
        }
        .instrument(gateway_span)
        .await;
    }
    Ok(())
}

async fn inbound_from_callback_query(
    query: &CallbackQuery,
    inbound_contexts: Arc<Mutex<HashMap<String, TelegramMessageRef>>>,
    outbound_messages: Arc<Mutex<HashMap<String, Vec<TelegramMessageRef>>>>,
) -> Option<InboundMessage> {
    let action = ApprovalCallbackAction::decode(query.data.as_deref()?)?;
    let origin = if let Some(message) = query.regular_message() {
        TelegramMessageRef::from_message(message)
    } else {
        let message_id = query.inline_message_id.as_ref()?;
        outbound_messages
            .lock()
            .await
            .get(message_id)
            .and_then(|refs| refs.last().copied())?
    };

    let platform_msg_id = format!("callback:{}", query.id);
    inbound_contexts
        .lock()
        .await
        .insert(platform_msg_id.clone(), origin);

    Some(InboundMessage {
        platform: Platform::Telegram,
        platform_msg_id,
        user: PlatformUser {
            platform_id: query.from.id.0.to_string(),
            display_name: telegram_user_name(&query.from),
            moa_user_id: None,
        },
        channel: channel_from_chat_and_reply(origin.chat_id, Some(origin.message_id), false),
        text: action.inbound_command(),
        attachments: Vec::new(),
        reply_to: Some(origin.message_id.to_string()),
        timestamp: Utc::now(),
    })
}

fn inbound_from_message(msg: &Message) -> Option<InboundMessage> {
    let text = msg
        .text()
        .map(ToOwned::to_owned)
        .or_else(|| msg.caption().map(ToOwned::to_owned))?;

    let reply_to = msg.reply_to_message().map(|reply| reply.id.0.to_string());
    let is_direct = msg.chat.is_private();
    Some(InboundMessage {
        platform: Platform::Telegram,
        platform_msg_id: msg.id.0.to_string(),
        user: PlatformUser {
            platform_id: msg.from.as_ref()?.id.0.to_string(),
            display_name: telegram_user_name(msg.from.as_ref()?),
            moa_user_id: None,
        },
        channel: channel_from_chat_and_reply(
            msg.chat.id.0,
            reply_to.as_deref().and_then(|id| id.parse().ok()),
            is_direct,
        ),
        text,
        attachments: Vec::new(),
        reply_to,
        timestamp: msg.date,
    })
}

fn channel_from_chat_and_reply(chat_id: i64, reply_to: Option<i32>, is_direct: bool) -> ChannelRef {
    if is_direct {
        return ChannelRef::DirectMessage {
            user_id: chat_id.to_string(),
        };
    }

    if let Some(reply_to) = reply_to {
        ChannelRef::Thread {
            channel_id: chat_id.to_string(),
            thread_id: reply_to.to_string(),
        }
    } else {
        ChannelRef::Group {
            channel_id: chat_id.to_string(),
        }
    }
}

fn parse_message_id(value: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .map_err(|_| MoaError::ValidationError(format!("invalid telegram message id: {value}")))
}

fn inline_keyboard(buttons: &[moa_core::ActionButton]) -> Option<InlineKeyboardMarkup> {
    if buttons.is_empty() {
        return None;
    }

    let rows = buttons
        .chunks(3)
        .map(|row| {
            row.iter()
                .map(|button| {
                    InlineKeyboardButton::callback(
                        button.label.clone(),
                        button.callback_data.clone(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    Some(InlineKeyboardMarkup::new(rows))
}

fn telegram_user_name(user: &User) -> String {
    if let Some(username) = user.username.clone() {
        return format!("@{username}");
    }

    match &user.last_name {
        Some(last_name) => format!("{} {}", user.first_name, last_name),
        None => user.first_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_inbound_message_from_update_payload() {
        let message: Message = serde_json::from_value(json!({
            "message_id": 42,
            "date": 1712668800,
            "chat": {
                "id": 123456,
                "type": "private",
                "first_name": "Alice"
            },
            "from": {
                "id": 123456,
                "is_bot": false,
                "first_name": "Alice",
                "username": "alice"
            },
            "text": "hello moa"
        }))
        .expect("telegram message should deserialize");

        let inbound = inbound_from_message(&message).expect("normalized message");
        assert_eq!(inbound.platform, Platform::Telegram);
        assert_eq!(inbound.platform_msg_id, "42");
        assert_eq!(inbound.text, "hello moa");
        assert_eq!(inbound.reply_to, None);
        assert_eq!(
            inbound.channel,
            ChannelRef::DirectMessage {
                user_id: "123456".to_string()
            }
        );
    }

    #[tokio::test]
    async fn parses_approval_callback_into_control_message() {
        let request_id = Uuid::now_v7();
        let query: CallbackQuery = serde_json::from_value(json!({
            "id": "cb-1",
            "from": {
                "id": 123456,
                "is_bot": false,
                "first_name": "Alice",
                "username": "alice"
            },
            "chat_instance": "instance-1",
            "data": ApprovalCallbackAction::AlwaysAllow { request_id }.encode(),
            "message": {
                "message_id": 77,
                "date": 1712668800,
                "chat": {
                    "id": -100987654,
                    "type": "group",
                    "title": "MOA"
                },
                "from": {
                    "id": 999,
                    "is_bot": true,
                    "first_name": "MOA Bot",
                    "username": "moa_bot"
                },
                "text": "approval"
            }
        }))
        .expect("callback query should deserialize");

        let inbound = inbound_from_callback_query(
            &query,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .await
        .expect("normalized callback");

        assert_eq!(inbound.platform, Platform::Telegram);
        assert_eq!(inbound.reply_to, Some("77".to_string()));
        assert_eq!(inbound.text, format!("/approval always {request_id}"));
        assert_eq!(
            inbound.channel,
            ChannelRef::Thread {
                channel_id: "-100987654".to_string(),
                thread_id: "77".to_string(),
            }
        );
    }
}
