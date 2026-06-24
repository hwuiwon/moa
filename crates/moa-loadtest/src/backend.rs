//! Remote load-test backend adapter.

use crate::*;
use moa_authz::{FgaClient, FgaConfig};
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{
    AppendEventRequest, GetEventsRequest, InitSessionVoRequest, SessionSnapshot, StartTurnRequest,
    TurnOutcome,
};
use serde::Serialize;

const SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[async_trait]
pub(crate) trait SessionTarget: Send + Sync {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId>;
    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation>;
    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta>;
    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>>;
    async fn cleanup(&self) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct RemoteTarget {
    client: RemoteHttpClient,
    fga: FgaClient,
    identity: Identity,
    workspace_id: WorkspaceId,
    tenant_id: TenantId,
    model: ModelId,
}

#[async_trait]
impl SessionTarget for RemoteTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
        self.grant_workspace_member().await?;
        let session_id = SessionId::new();
        let now = chrono::Utc::now();
        let meta = SessionMeta {
            id: session_id,
            tenant_id: self.tenant_id,
            title: Some(plan.title.clone()),
            status: SessionStatus::Created,
            channel: Channel::Chat,
            active_channel_binding_id: None,
            model: self.model.clone(),
            created_at: now,
            updated_at: now,
            completed_at: None,
            parent_session_id: None,
            contact: None,
            created_by: Some(SessionActorRef::Identity {
                id: self.identity.id,
            }),
            contact_promoted_from_id: None,
            agent_context: Some(moa_core::AgentContext::system_default()),
            total_input_tokens: 0,
            total_input_tokens_uncached: 0,
            total_input_tokens_cache_write: 0,
            total_input_tokens_cache_read: 0,
            total_output_tokens: 0,
            total_cost_cents: 0,
            event_count: 0,
            last_checkpoint_seq: None,
        };

        self.client
            .create_session(meta.clone())
            .await
            .map_err(client_error)?;
        self.grant_session_participant(session_id).await?;
        self.client
            .append_event(
                session_id,
                Event::SessionCreated {
                    tenant_id: self.tenant_id,
                    contact_id: None,
                    created_by: Some(SessionActorRef::Identity {
                        id: self.identity.id,
                    }),
                    model: self.model.clone(),
                    channel: Channel::Chat,
                },
            )
            .await
            .map_err(client_error)?;
        self.client
            .init_session_vo(session_id, meta)
            .await
            .map_err(client_error)?;

        Ok(session_id)
    }

    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> Result<TurnObservation> {
        let started = Instant::now();
        let handle = self.client.session(session_id.to_string());
        let response = handle
            .start_turn(
                StartTurnRequest {
                    user_message: prompt.to_string(),
                    attachments: Vec::new(),
                    model: Some(self.model.to_string()),
                    contact: None,
                    max_turns: None,
                },
                Some(&format!("loadtest-turn-{session_id}-{}", Uuid::now_v7())),
            )
            .await
            .map_err(client_error)?;
        let turn_id = response.turn_id.ok_or_else(|| {
            MoaError::ProviderError(format!(
                "loadtest turn for session {session_id} queued unexpectedly"
            ))
        })?;
        let outcome = handle
            .await_turn_outcome(&turn_id, timeout, SNAPSHOT_POLL_INTERVAL)
            .await
            .map_err(client_error)?;

        match outcome.kind {
            moa_core::wire::TurnOutcomeKind::Completed => Ok(TurnObservation {
                latency: started.elapsed(),
                ttft: None,
                auto_denied_approvals: 0,
            }),
            moa_core::wire::TurnOutcomeKind::Cancelled => Err(MoaError::Cancelled),
            moa_core::wire::TurnOutcomeKind::Failed => {
                Err(MoaError::ProviderError(outcome.message))
            }
        }
    }

    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.client
            .get_session(session_id)
            .await
            .map_err(client_error)
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        self.client
            .get_events(session_id, moa_core::EventRange::all())
            .await
            .map_err(client_error)
    }

    async fn cleanup(&self) -> Result<()> {
        Ok(())
    }
}

impl RemoteTarget {
    async fn grant_workspace_member(&self) -> Result<()> {
        self.grant_raw_tuple(
            format!("user:{}", self.identity.id),
            "member",
            format!("workspace:{}", self.workspace_id),
        )
        .await
    }

    async fn grant_session_participant(&self, session_id: SessionId) -> Result<()> {
        self.grant_raw_tuple(
            format!("user:{}", self.identity.id),
            "participant",
            format!("session:{session_id}"),
        )
        .await
    }

    async fn grant_raw_tuple(&self, user: String, relation: &str, object: String) -> Result<()> {
        self.fga
            .apply_raw(serde_json::json!({
                "authorization_model_id": self.fga.model_id(),
                "writes": {
                    "tuple_keys": [{
                        "user": user,
                        "relation": relation,
                        "object": object,
                    }],
                },
            }))
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("loadtest OpenFGA grant failed: {error}"))
            })
    }
}

pub(crate) async fn build_backend(
    options: &LoadTestOptions,
    config: &MoaConfig,
) -> Result<Arc<dyn SessionTarget>> {
    let tenant_id = TenantId::new();
    let identity = Identity {
        identity_type: IdentityType::User,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let client = RemoteHttpClient::new(&options.endpoint)
        .map_err(client_error)?
        .with_identity(identity.clone());
    let fga = live_fga_client()?;
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| config.model_for_task(ModelTask::MainLoop).to_string());
    Ok(Arc::new(RemoteTarget {
        client,
        fga,
        identity,
        workspace_id: WorkspaceId::new(tenant_id.to_string()),
        tenant_id,
        model: ModelId::new(model),
    }))
}

fn client_error(error: RemoteHttpError) -> MoaError {
    MoaError::ProviderError(format!("orchestrator client error: {error}"))
}

#[derive(Clone)]
struct RemoteHttpClient {
    endpoint: String,
    http: reqwest::Client,
    identity: Option<Identity>,
}

impl RemoteHttpClient {
    fn new(endpoint: &str) -> std::result::Result<Self, RemoteHttpError> {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        url::Url::parse(&endpoint)
            .map_err(|error| RemoteHttpError::InvalidEndpoint(format!("{endpoint}: {error}")))?;
        Ok(Self {
            endpoint,
            http: reqwest::Client::new(),
            identity: None,
        })
    }

    fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    async fn create_session(
        &self,
        meta: SessionMeta,
    ) -> std::result::Result<SessionId, RemoteHttpError> {
        self.post_call("/SessionStore/create_session", &meta).await
    }

    async fn init_session_vo(
        &self,
        session_id: SessionId,
        meta: SessionMeta,
    ) -> std::result::Result<(), RemoteHttpError> {
        self.post_void(
            "/SessionStore/init_session_vo",
            &InitSessionVoRequest { session_id, meta },
        )
        .await
    }

    async fn append_event(
        &self,
        session_id: SessionId,
        event: Event,
    ) -> std::result::Result<u64, RemoteHttpError> {
        self.post_call(
            "/SessionStore/append_event",
            &AppendEventRequest { session_id, event },
        )
        .await
    }

    async fn get_session(
        &self,
        session_id: SessionId,
    ) -> std::result::Result<SessionMeta, RemoteHttpError> {
        self.post_call("/SessionStore/get_session", &session_id)
            .await
    }

    async fn get_events(
        &self,
        session_id: SessionId,
        range: moa_core::EventRange,
    ) -> std::result::Result<Vec<EventRecord>, RemoteHttpError> {
        self.post_call(
            "/SessionStore/get_events",
            &GetEventsRequest { session_id, range },
        )
        .await
    }

    fn session(&self, session_id: String) -> RemoteSessionHandle<'_> {
        RemoteSessionHandle {
            client: self,
            session_id,
        }
    }

    async fn post_call<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
    ) -> std::result::Result<Resp, RemoteHttpError>
    where
        Req: Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        self.post_call_with_idempotency(path, body, None).await
    }

    async fn post_call_with_idempotency<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
        idempotency_key: Option<&str>,
    ) -> std::result::Result<Resp, RemoteHttpError>
    where
        Req: Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        let mut request = self.apply_auth(
            self.http
                .post(format!("{}{path}", self.endpoint))
                .json(body),
        );
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        let response = request.send().await?;
        decode_response(response).await
    }

    async fn post_empty_call<Resp>(&self, path: &str) -> std::result::Result<Resp, RemoteHttpError>
    where
        Resp: serde::de::DeserializeOwned,
    {
        let response = self
            .apply_auth(self.http.post(format!("{}{path}", self.endpoint)))
            .send()
            .await?;
        decode_response(response).await
    }

    async fn post_void<Req>(
        &self,
        path: &str,
        body: &Req,
    ) -> std::result::Result<(), RemoteHttpError>
    where
        Req: Serialize + ?Sized,
    {
        let response = self
            .apply_auth(
                self.http
                    .post(format!("{}{path}", self.endpoint))
                    .json(body),
            )
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(RemoteHttpError::BadStatus { status, body });
        }
        Ok(())
    }

    fn apply_auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let Some(identity) = &self.identity else {
            return request;
        };
        let identity_type = match identity.identity_type {
            IdentityType::User => "user",
            IdentityType::Agent => "agent",
            IdentityType::Service => "service",
            IdentityType::Contact => "contact",
        };
        let mut request = request
            .header("x-moa-identity-type", identity_type)
            .header("x-moa-identity-id", identity.id.to_string())
            .header("x-moa-tenant-id", identity.tenant_id.to_string());
        if let Some(api_key_id) = identity.api_key_id {
            request = request.header("x-moa-api-key-id", api_key_id.to_string());
        }
        if let Some(user_id) = identity.acting_on_behalf_of {
            request = request.header("x-moa-acting-on-behalf-of", user_id.to_string());
        }
        request
    }
}

struct RemoteSessionHandle<'a> {
    client: &'a RemoteHttpClient,
    session_id: String,
}

impl RemoteSessionHandle<'_> {
    async fn start_turn(
        &self,
        request: StartTurnRequest,
        idempotency_key: Option<&str>,
    ) -> std::result::Result<moa_core::wire::StartTurnResponse, RemoteHttpError> {
        self.client
            .post_call_with_idempotency(
                &format!("/Session/{}/start_turn", self.session_id),
                &request,
                idempotency_key,
            )
            .await
    }

    async fn snapshot(&self) -> std::result::Result<SessionSnapshot, RemoteHttpError> {
        self.client
            .post_empty_call(&format!("/Session/{}/snapshot", self.session_id))
            .await
    }

    async fn await_turn_outcome(
        &self,
        turn_id: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> std::result::Result<TurnOutcome, RemoteHttpError> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot().await?;
            if let Some(outcome) = snapshot.last_outcome
                && outcome.turn_id == turn_id
            {
                return Ok(outcome);
            }
            if Instant::now() >= deadline {
                return Err(RemoteHttpError::Timeout(timeout));
            }
            tokio::time::sleep(poll_interval).await;
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum RemoteHttpError {
    #[error("orchestrator endpoint URL invalid: {0}")]
    InvalidEndpoint(String),
    #[error("network error talking to orchestrator: {0}")]
    Network(#[from] reqwest::Error),
    #[error("orchestrator returned bad status {status}: {body}")]
    BadStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("failed to decode orchestrator response: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("operation timed out after {0:?}")]
    Timeout(Duration),
}

async fn decode_response<Resp>(
    response: reqwest::Response,
) -> std::result::Result<Resp, RemoteHttpError>
where
    Resp: serde::de::DeserializeOwned,
{
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(RemoteHttpError::BadStatus { status, body });
    }
    Ok(serde_json::from_str(&body)?)
}

fn live_fga_client() -> Result<FgaClient> {
    FgaClient::new(FgaConfig {
        url: std::env::var("MOA_AUTHZ_OPENFGA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:10030".to_string()),
        preshared_key: std::env::var("MOA_AUTHZ_OPENFGA_PRESHARED_KEY")
            .unwrap_or_else(|_| "localdev-preshared-key-do-not-use-in-prod".to_string()),
        store_id: std::env::var("MOA_AUTHZ_OPENFGA_STORE_ID").map_err(|_| {
            MoaError::MissingEnvironmentVariable("MOA_AUTHZ_OPENFGA_STORE_ID".to_string())
        })?,
        model_id: std::env::var("MOA_AUTHZ_OPENFGA_MODEL_ID").map_err(|_| {
            MoaError::MissingEnvironmentVariable("MOA_AUTHZ_OPENFGA_MODEL_ID".to_string())
        })?,
        timeout_ms: 5_000,
    })
    .map_err(|error| MoaError::ProviderError(format!("OpenFGA client config: {error}")))
}
