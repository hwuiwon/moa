//! Remote load-test backend adapter.

use crate::*;
use moa_authz::{FgaClient, FgaConfig};
use moa_core::traits::Identity;
use moa_wire::session_store::{AppendEventRequest, GetEventsRequest, InitSessionVoRequest};
use moa_wire::turn::{SessionSnapshot, StartTurnRequest, TurnOutcome};
use serde::Serialize;

const SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Classification of a failed turn attempt, used by the error taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnFailureKind {
    /// The start_turn request itself failed.
    StartFailed,
    /// The turn did not reach an outcome within the per-turn timeout.
    Timeout,
    /// The turn reached a Failed outcome.
    Failed,
    /// The turn reached a Cancelled outcome.
    Cancelled,
    /// Transport-level failure while awaiting the outcome.
    Transport,
}

/// A failed turn attempt with its classification.
#[derive(Debug, Clone)]
pub(crate) struct TurnFailure {
    pub(crate) kind: TurnFailureKind,
    pub(crate) message: String,
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

#[async_trait]
pub(crate) trait SessionTarget: Send + Sync {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId>;
    async fn run_turn(
        &self,
        session_id: SessionId,
        prompt: &str,
        timeout: Duration,
    ) -> std::result::Result<TurnObservation, TurnFailure>;
    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta>;
    /// Events with `sequence_num` strictly greater than `after_seq`.
    async fn session_events_since(
        &self,
        session_id: SessionId,
        after_seq: u64,
    ) -> Result<Vec<EventRecord>>;
    /// A recent suffix of the event log, for failure-note extraction.
    async fn recent_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>>;
}

#[derive(Clone)]
pub(crate) struct RemoteTarget {
    client: RemoteHttpClient,
    fga: FgaClient,
    identity: Identity,
    tenant_id: TenantId,
    model: ModelId,
}

#[async_trait]
impl SessionTarget for RemoteTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
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
            agent_context: Some(moa_core::types::agent::AgentContext::system_default()),
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
    ) -> std::result::Result<TurnObservation, TurnFailure> {
        let handle = self.client.session(session_id.to_string());
        let response = handle
            .start_turn(
                StartTurnRequest {
                    user_message: prompt.to_string(),
                    attachments: Vec::new(),
                    model: Some(self.model.to_string()),
                    contact: None,
                    max_turns: None,
                    execution_template: None,
                },
                Some(&format!("loadtest-turn-{session_id}-{}", Uuid::now_v7())),
            )
            .await
            .map_err(|error| TurnFailure {
                kind: TurnFailureKind::StartFailed,
                message: error.to_string(),
            })?;
        let turn_id = response.turn_id.ok_or_else(|| TurnFailure {
            kind: TurnFailureKind::StartFailed,
            message: format!("loadtest turn for session {session_id} queued unexpectedly"),
        })?;
        let outcome = handle
            .await_turn_outcome(&turn_id, timeout, SNAPSHOT_POLL_INTERVAL)
            .await
            .map_err(|error| TurnFailure {
                kind: match error {
                    RemoteHttpError::Timeout(_) => TurnFailureKind::Timeout,
                    _ => TurnFailureKind::Transport,
                },
                message: error.to_string(),
            })?;

        classify_turn_outcome(outcome)
    }

    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.client
            .get_session(session_id)
            .await
            .map_err(client_error)
    }

    async fn session_events_since(
        &self,
        session_id: SessionId,
        after_seq: u64,
    ) -> Result<Vec<EventRecord>> {
        self.client
            .get_events(
                session_id,
                moa_core::types::events_stream::EventRange {
                    from_seq: Some(after_seq + 1),
                    ..moa_core::types::events_stream::EventRange::default()
                },
            )
            .await
            .map_err(client_error)
    }

    async fn recent_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        self.client
            .get_events(
                session_id,
                moa_core::types::events_stream::EventRange::recent(50),
            )
            .await
            .map_err(client_error)
    }
}

fn classify_turn_outcome(
    outcome: TurnOutcome,
) -> std::result::Result<TurnObservation, TurnFailure> {
    match outcome.kind {
        moa_wire::turn::TurnOutcomeKind::Completed => Ok(TurnObservation {
            kind: TurnObservationKind::CompletedAnswer,
            ttft: None,
            edge_observation_wait: None,
            auto_denied_approvals: 0,
        }),
        moa_wire::turn::TurnOutcomeKind::Accepted { execution_run_uid } => Ok(TurnObservation {
            kind: TurnObservationKind::ExecutionAdmission {
                run_uid: execution_run_uid,
            },
            ttft: None,
            edge_observation_wait: None,
            auto_denied_approvals: 0,
        }),
        moa_wire::turn::TurnOutcomeKind::Cancelled => Err(TurnFailure {
            kind: TurnFailureKind::Cancelled,
            message: outcome.message,
        }),
        moa_wire::turn::TurnOutcomeKind::Failed => Err(TurnFailure {
            kind: TurnFailureKind::Failed,
            message: outcome.message,
        }),
    }
}

impl RemoteTarget {
    /// Builds an ingress-backed target for one identity.
    pub(crate) fn new(
        endpoint: &str,
        http: reqwest::Client,
        fga: FgaClient,
        identity: Identity,
        tenant_id: TenantId,
        model: ModelId,
    ) -> std::result::Result<Self, String> {
        let client = RemoteHttpClient::new_with_http(endpoint, http)
            .map_err(|error| error.to_string())?
            .with_identity(identity.clone());
        Ok(Self {
            client,
            fga,
            identity,
            tenant_id,
            model,
        })
    }

    async fn grant_session_participant(&self, session_id: SessionId) -> Result<()> {
        grant_raw_tuple(
            &self.fga,
            format!("operator:{}", self.identity.id),
            "participant",
            format!("session:{session_id}"),
        )
        .await
    }
}

/// Builds one target per pool identity, sharing HTTP and FGA clients, and
/// grants tenant-operator tuples exactly once.
pub(crate) async fn build_backend_pool(
    options: &LoadTestOptions,
    config: &MoaConfig,
    pool: &TenancyPool,
) -> Result<Vec<Arc<dyn SessionTarget>>> {
    let fga = live_fga_client()?;
    pool.grant_operators(&fga).await?;
    let http = build_http_client(options)?;
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| config.model_for_task(ModelTask::MainLoop).to_string());

    pool.entries()
        .iter()
        .map(|entry| {
            let client = RemoteHttpClient::new_with_http(&options.endpoint, http.clone())?
                .with_identity(entry.identity.clone());
            Ok(Arc::new(RemoteTarget {
                client,
                fga: fga.clone(),
                identity: entry.identity.clone(),
                tenant_id: entry.tenant_id,
                model: ModelId::new(model.clone()),
            }) as Arc<dyn SessionTarget>)
        })
        .collect::<std::result::Result<Vec<_>, RemoteHttpError>>()
        .map_err(client_error)
}

fn client_error(error: RemoteHttpError) -> MoaError {
    MoaError::ProviderError(format!("orchestrator client error: {error}"))
}

/// Shared HTTP client with a hard request deadline slightly above the turn
/// timeout, so a hung connection can never wedge a generator slot.
pub(crate) fn build_http_client(options: &LoadTestOptions) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(options.turn_timeout.saturating_add(Duration::from_secs(30)))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| MoaError::ProviderError(format!("loadtest HTTP client: {error}")))
}

#[derive(Clone)]
struct RemoteHttpClient {
    endpoint: String,
    http: reqwest::Client,
    identity: Option<Identity>,
}

impl RemoteHttpClient {
    fn new_with_http(
        endpoint: &str,
        http: reqwest::Client,
    ) -> std::result::Result<Self, RemoteHttpError> {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        url::Url::parse(&endpoint)
            .map_err(|error| RemoteHttpError::InvalidEndpoint(format!("{endpoint}: {error}")))?;
        Ok(Self {
            endpoint,
            http,
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
        self.post_call::<_, EventRecord>(
            "/SessionStore/append_event",
            &AppendEventRequest {
                session_id,
                event,
                dedupe_key: None,
            },
        )
        .await
        .map(|record| record.sequence_num)
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
        range: moa_core::types::events_stream::EventRange,
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
                .post(format!("{}/restate/call{path}", self.endpoint))
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
            .apply_auth(
                self.http
                    .post(format!("{}/restate/call{path}", self.endpoint)),
            )
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
                    .post(format!("{}/restate/call{path}", self.endpoint))
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
        let mut request = request
            .header("x-moa-identity-type", identity.identity_type.as_str())
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
    ) -> std::result::Result<moa_wire::turn::StartTurnResponse, RemoteHttpError> {
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

pub(crate) fn live_fga_client() -> Result<FgaClient> {
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

#[cfg(test)]
mod tests {
    use moa_wire::turn::TurnOutcomeKind;

    use super::*;

    #[test]
    fn accepted_execution_run_direct_backend_is_successful_admission() {
        // Pins: the direct Restate backend treats Accepted as a successful admission, never as a
        // completed answer or failed turn, and preserves the durable run identifier.
        let run_uid = Uuid::now_v7();
        let observation = classify_turn_outcome(TurnOutcome {
            turn_id: "turn-1".to_string(),
            kind: TurnOutcomeKind::Accepted {
                execution_run_uid: run_uid,
            },
            message: "execution accepted".to_string(),
        })
        .expect("Accepted should be a successful backend result");

        assert_eq!(
            observation.kind,
            TurnObservationKind::ExecutionAdmission { run_uid }
        );
        assert!(observation.ttft.is_none());
    }
}
