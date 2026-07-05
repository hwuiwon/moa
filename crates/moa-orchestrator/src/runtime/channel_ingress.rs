//! Runtime channel-adapter ingress for narrow session commands.

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use moa_core::{
    Channel, ChannelEvent, ChannelSessionCommand, InboundMessage, MessageContent, MoaError,
    OutboundMessage, Result, SessionChannelBindingResolution, SessionId,
    traits::{ChannelAdapter, Identity, IdentityType, SessionChannelStore},
    wire::turn::{CancelResponse, SessionProgress, SessionProgressRequest},
};
use reqwest::Client;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::restate_identity::with_reqwest_identity_headers;

const CHANNEL_EVENT_BUFFER: usize = 128;
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Starts configured channel adapters and routes narrow session commands.
#[must_use]
pub fn spawn_channel_ingress(
    adapters: HashMap<Channel, Arc<dyn ChannelAdapter>>,
    session_store: Arc<dyn SessionChannelStore>,
    restate_ingress_url: impl Into<String>,
    shutdown: CancellationToken,
) -> Option<JoinHandle<()>> {
    if adapters.is_empty() {
        return None;
    }
    let transport = match RestateSessionCommandTransport::new(restate_ingress_url) {
        Ok(transport) => Arc::new(transport),
        Err(error) => {
            tracing::warn!(error = %error, "channel ingress disabled");
            return None;
        }
    };
    Some(tokio::spawn(run_channel_ingress(
        adapters,
        session_store,
        transport,
        shutdown,
    )))
}

async fn run_channel_ingress(
    adapters: HashMap<Channel, Arc<dyn ChannelAdapter>>,
    session_store: Arc<dyn SessionChannelStore>,
    transport: Arc<dyn SessionCommandTransport>,
    shutdown: CancellationToken,
) {
    let (event_tx, mut event_rx) = mpsc::channel(CHANNEL_EVENT_BUFFER);
    let mut adapter_tasks = Vec::with_capacity(adapters.len());
    for adapter in adapters.values() {
        let adapter = adapter.clone();
        let event_tx = event_tx.clone();
        adapter_tasks.push(tokio::spawn(async move {
            if let Err(error) = adapter.start(event_tx).await {
                tracing::warn!(channel = %adapter.channel(), error = %error, "channel adapter stopped");
            }
        }));
    }
    drop(event_tx);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            event = event_rx.recv() => {
                let Some(event) = event else {
                    tracing::warn!("all channel adapters stopped; channel ingress exiting");
                    break;
                };
                if let Err(error) = handle_channel_event(
                    event,
                    session_store.as_ref(),
                    &adapters,
                    transport.as_ref(),
                ).await {
                    tracing::warn!(error = %error, "channel event handling failed");
                }
            }
        }
    }

    for task in adapter_tasks {
        task.abort();
    }
}

async fn handle_channel_event(
    event: ChannelEvent,
    session_store: &dyn SessionChannelStore,
    adapters: &HashMap<Channel, Arc<dyn ChannelAdapter>>,
    transport: &dyn SessionCommandTransport,
) -> Result<()> {
    let ChannelEvent::SessionCommand(command) = event else {
        return Ok(());
    };
    let (kind, inbound) = CommandKind::from_command(&command);
    let Some(adapter) = adapters.get(&inbound.channel) else {
        return Err(MoaError::ProviderError(format!(
            "no channel adapter configured for {}",
            inbound.channel.as_str()
        )));
    };
    let Some(resolution) = session_store
        .get_active_session_binding_for_channel(&inbound.channel_ref)
        .await?
    else {
        send_reply(
            adapter.as_ref(),
            inbound,
            &inbound.channel_ref,
            "No active MOA session is bound to this conversation.".to_string(),
        )
        .await?;
        return Ok(());
    };

    let identity = identity_for_resolution(&resolution);
    let reply = match kind {
        CommandKind::Status => {
            let progress = transport.progress(&identity, resolution.session_id).await?;
            status_reply(&progress)
        }
        CommandKind::Stop => {
            let response = transport
                .request_cancel(
                    &identity,
                    resolution.session_id,
                    "channel session stop requested",
                )
                .await?;
            cancel_reply(&response)
        }
    };
    send_reply(
        adapter.as_ref(),
        inbound,
        &resolution.binding.channel_ref,
        reply,
    )
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Status,
    Stop,
}

impl CommandKind {
    fn from_command(command: &ChannelSessionCommand) -> (Self, &InboundMessage) {
        match command {
            ChannelSessionCommand::Status(inbound) => (Self::Status, inbound),
            ChannelSessionCommand::Stop(inbound) => (Self::Stop, inbound),
        }
    }
}

fn identity_for_resolution(resolution: &SessionChannelBindingResolution) -> Identity {
    Identity {
        identity_type: IdentityType::Contact,
        id: resolution.contact_id.0,
        tenant_id: resolution.tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

async fn send_reply(
    adapter: &dyn ChannelAdapter,
    inbound: &InboundMessage,
    channel_ref: &moa_core::ChannelRef,
    text: String,
) -> Result<()> {
    adapter
        .send(OutboundMessage {
            content: MessageContent::Markdown(text),
            channel_ref: Some(channel_ref.clone()),
            reply_to: Some(inbound.channel_msg_id.clone()),
            ephemeral: false,
        })
        .await?;
    Ok(())
}

fn status_reply(progress: &SessionProgress) -> String {
    if let Some(turn) = progress.active_turn_progress.as_ref() {
        let summary = turn
            .last_progress_summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or("Working on it.");
        return format!(
            "Session {} is {:?}. {summary}",
            progress.snapshot.session_id, turn.phase
        );
    }
    if progress.snapshot.pending_message_count > 0 {
        return format!(
            "Session {} has no active turn; {} message(s) are queued.",
            progress.snapshot.session_id, progress.snapshot.pending_message_count
        );
    }
    format!(
        "Session {} has no active turn.",
        progress.snapshot.session_id
    )
}

fn cancel_reply(response: &CancelResponse) -> String {
    if response.cancelled {
        "Cancel requested for the active turn.".to_string()
    } else {
        format!("No active turn was cancelled: {}", response.reason)
    }
}

#[async_trait]
trait SessionCommandTransport: Send + Sync {
    async fn progress(&self, identity: &Identity, session_id: SessionId)
    -> Result<SessionProgress>;

    async fn request_cancel(
        &self,
        identity: &Identity,
        session_id: SessionId,
        reason: &str,
    ) -> Result<CancelResponse>;
}

struct RestateSessionCommandTransport {
    http: Client,
    base_url: String,
}

impl RestateSessionCommandTransport {
    fn new(restate_ingress_url: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(Self {
            http,
            base_url: restate_ingress_url.into().trim_end_matches('/').to_string(),
        })
    }

    async fn post_json<Req, Res>(
        &self,
        identity: &Identity,
        session_id: SessionId,
        handler: &str,
        body: &Req,
    ) -> Result<Res>
    where
        Req: Serialize + Sync,
        Res: DeserializeOwned,
    {
        let url = format!(
            "{}/restate/call/Session/{session_id}/{handler}",
            self.base_url
        );
        let response = with_reqwest_identity_headers(self.http.post(url).json(body), identity)
            .send()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MoaError::ProviderError(format!(
                "Session/{handler} failed with {status}: {body}"
            )));
        }
        response
            .json::<Res>()
            .await
            .map_err(|error| MoaError::SerializationError(error.to_string()))
    }
}

#[async_trait]
impl SessionCommandTransport for RestateSessionCommandTransport {
    async fn progress(
        &self,
        identity: &Identity,
        session_id: SessionId,
    ) -> Result<SessionProgress> {
        self.post_json(
            identity,
            session_id,
            "progress",
            &SessionProgressRequest::default(),
        )
        .await
    }

    async fn request_cancel(
        &self,
        identity: &Identity,
        session_id: SessionId,
        reason: &str,
    ) -> Result<CancelResponse> {
        self.post_json(identity, session_id, "request_cancel", &reason.to_string())
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use moa_core::{
        ChannelActor, ChannelCapabilities, ChannelRef, ContactId, MessageId, SessionChannelBinding,
        SessionChannelBindingId, TenantId,
        wire::turn::{SessionSnapshot, TurnComplexityClass, TurnPhase, TurnProgress},
    };
    use uuid::Uuid;

    use super::*;

    struct FakeSessionChannelStore {
        resolution: Option<SessionChannelBindingResolution>,
    }

    #[async_trait]
    impl SessionChannelStore for FakeSessionChannelStore {
        async fn replace_session_channel_binding(
            &self,
            _update: moa_core::traits::SessionChannelBindingUpdate,
        ) -> Result<SessionChannelBindingId> {
            Err(MoaError::Unsupported("not used in test".to_string()))
        }

        async fn get_active_session_channel_binding(
            &self,
            _session_id: SessionId,
        ) -> Result<Option<SessionChannelBinding>> {
            Err(MoaError::Unsupported("not used in test".to_string()))
        }

        async fn get_active_session_binding_for_channel(
            &self,
            _channel_ref: &ChannelRef,
        ) -> Result<Option<SessionChannelBindingResolution>> {
            Ok(self.resolution.clone())
        }
    }

    struct FakeChannelAdapter {
        sent: Mutex<Vec<OutboundMessage>>,
    }

    impl FakeChannelAdapter {
        fn sent(&self) -> Vec<OutboundMessage> {
            self.sent.lock().expect("sent lock").clone()
        }
    }

    #[async_trait]
    impl ChannelAdapter for FakeChannelAdapter {
        fn channel(&self) -> Channel {
            Channel::Slack
        }

        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities {
                max_message_length: 40_000,
                supports_ephemeral: false,
                supports_threads: true,
                supports_code_blocks: true,
                supports_edit: true,
                supports_reactions: false,
                min_edit_interval: Duration::ZERO,
            }
        }

        async fn start(&self, _event_tx: mpsc::Sender<ChannelEvent>) -> Result<()> {
            Ok(())
        }

        async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
            self.sent.lock().expect("sent lock").push(msg);
            Ok(MessageId::new("reply"))
        }

        async fn edit(&self, _msg_id: &MessageId, _msg: OutboundMessage) -> Result<()> {
            Ok(())
        }

        async fn delete(&self, _msg_id: &MessageId) -> Result<()> {
            Ok(())
        }
    }

    struct FakeSessionCommandTransport {
        progress_calls: AtomicUsize,
        cancel_calls: AtomicUsize,
    }

    #[async_trait]
    impl SessionCommandTransport for FakeSessionCommandTransport {
        async fn progress(
            &self,
            _identity: &Identity,
            session_id: SessionId,
        ) -> Result<SessionProgress> {
            self.progress_calls.fetch_add(1, Ordering::SeqCst);
            Ok(SessionProgress {
                snapshot: SessionSnapshot {
                    session_id: session_id.to_string(),
                    active_turn_id: Some("turn-1".to_string()),
                    pending_message_count: 0,
                    last_outcome: None,
                },
                active_turn_progress: Some(TurnProgress {
                    turn_id: "turn-1".to_string(),
                    phase: TurnPhase::Tooling,
                    complexity_class: TurnComplexityClass::Standard,
                    iteration: 1,
                    max_turns: None,
                    tool_calls: 1,
                    max_tool_calls: None,
                    elapsed_ms: 42,
                    last_progress_summary: Some("Calling the model".to_string()),
                    cancel_requested: false,
                    cancel_reason: None,
                }),
                events: Vec::new(),
                child_progress: Vec::new(),
            })
        }

        async fn request_cancel(
            &self,
            _identity: &Identity,
            _session_id: SessionId,
            reason: &str,
        ) -> Result<CancelResponse> {
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(reason, "channel session stop requested");
            Ok(CancelResponse {
                cancelled: true,
                reason: "cancel forwarded".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn channel_status_command_calls_session_progress_and_sends_status() {
        // Pins: `/moa status` uses the real channel binding path and replies through the adapter.
        let channel_ref = slack_ref();
        let session_id = SessionId(
            Uuid::parse_str("11111111-1111-7111-8111-111111111111").expect("session id parses"),
        );
        let store = FakeSessionChannelStore {
            resolution: Some(resolution(session_id, channel_ref.clone())),
        };
        let adapter = Arc::new(FakeChannelAdapter {
            sent: Mutex::new(Vec::new()),
        });
        let mut adapters: HashMap<Channel, Arc<dyn ChannelAdapter>> = HashMap::new();
        adapters.insert(Channel::Slack, adapter.clone());
        let transport = FakeSessionCommandTransport {
            progress_calls: AtomicUsize::new(0),
            cancel_calls: AtomicUsize::new(0),
        };

        handle_channel_event(
            ChannelEvent::SessionCommand(ChannelSessionCommand::Status(inbound(
                "/moa status",
                channel_ref,
            ))),
            &store,
            &adapters,
            &transport,
        )
        .await
        .expect("status command handles");

        assert_eq!(transport.progress_calls.load(Ordering::SeqCst), 1);
        assert_eq!(transport.cancel_calls.load(Ordering::SeqCst), 0);
        let sent = adapter.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].reply_to.as_deref(), Some("1700000000.000200"));
        assert!(matches!(
            &sent[0].content,
            MessageContent::Markdown(text) if text.contains("Calling the model")
        ));
    }

    #[tokio::test]
    async fn channel_stop_command_requests_session_cancel_once() {
        // Pins: `/moa stop` forwards one cancel request and acknowledges through the adapter.
        let channel_ref = slack_ref();
        let session_id = SessionId(
            Uuid::parse_str("22222222-2222-7222-8222-222222222222").expect("session id parses"),
        );
        let store = FakeSessionChannelStore {
            resolution: Some(resolution(session_id, channel_ref.clone())),
        };
        let adapter = Arc::new(FakeChannelAdapter {
            sent: Mutex::new(Vec::new()),
        });
        let mut adapters: HashMap<Channel, Arc<dyn ChannelAdapter>> = HashMap::new();
        adapters.insert(Channel::Slack, adapter.clone());
        let transport = FakeSessionCommandTransport {
            progress_calls: AtomicUsize::new(0),
            cancel_calls: AtomicUsize::new(0),
        };

        handle_channel_event(
            ChannelEvent::SessionCommand(ChannelSessionCommand::Stop(inbound(
                "/moa stop",
                channel_ref,
            ))),
            &store,
            &adapters,
            &transport,
        )
        .await
        .expect("stop command handles");

        assert_eq!(transport.progress_calls.load(Ordering::SeqCst), 0);
        assert_eq!(transport.cancel_calls.load(Ordering::SeqCst), 1);
        let sent = adapter.sent();
        assert_eq!(sent.len(), 1);
        assert!(matches!(
            &sent[0].content,
            MessageContent::Markdown(text) if text.contains("Cancel requested")
        ));
    }

    fn slack_ref() -> ChannelRef {
        ChannelRef::Slack {
            team_id: Some("T123".to_string()),
            slack_channel_id: Some("C123".to_string()),
            thread_ts: Some("1700000000.000100".to_string()),
            user_id: Some("U123".to_string()),
        }
    }

    fn inbound(text: &str, channel_ref: ChannelRef) -> InboundMessage {
        InboundMessage {
            channel: Channel::Slack,
            channel_msg_id: "1700000000.000200".to_string(),
            actor: ChannelActor {
                external_id: "U123".to_string(),
                display_name: "<@U123>".to_string(),
                channel_account_id: None,
                moa_user_id: None,
            },
            channel_ref,
            text: text.to_string(),
            attachments: Vec::new(),
            reply_to: Some("1700000000.000100".to_string()),
            timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0)
                .expect("timestamp parses"),
        }
    }

    fn resolution(
        session_id: SessionId,
        channel_ref: ChannelRef,
    ) -> SessionChannelBindingResolution {
        SessionChannelBindingResolution {
            tenant_id: TenantId::from(
                Uuid::parse_str("33333333-3333-7333-8333-333333333333").expect("tenant id parses"),
            ),
            session_id,
            contact_id: ContactId(
                Uuid::parse_str("44444444-4444-7444-8444-444444444444").expect("contact id parses"),
            ),
            binding: SessionChannelBinding {
                binding_id: SessionChannelBindingId::new(),
                channel_ref,
            },
        }
    }
}
