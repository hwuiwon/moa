//! Top-level orchestrator HTTP client and configuration.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{
    AppendEventRequest, GetEventsRequest, InitSessionVoRequest, ListSessionsRequest,
    ToolDescriptor, WorkspaceCostSinceRequest,
};
use moa_core::{
    Event, EventRange, EventRecord, SessionFilter, SessionId, SessionMeta, SessionSummary,
    WorkspaceId,
};
use serde::Serialize;
use tracing::instrument;

use crate::error::{Error, Result};
use crate::session::SessionHandle;

const DEFAULT_ENDPOINT: &str = "http://localhost:10010";

/// Configuration for the thin orchestrator HTTP client.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Restate ingress endpoint that fronts the orchestrator deployment.
    pub endpoint: String,
    /// Timeout applied to each HTTP request.
    pub request_timeout: Duration,
    /// Timeout applied while establishing a new HTTP connection.
    pub connect_timeout: Duration,
    /// Maximum idle pooled connections retained per host.
    pub max_idle_per_host: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_string(),
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            max_idle_per_host: 16,
        }
    }
}

/// Thin HTTP client for Restate ingress calls into `moa-orchestrator`.
#[derive(Clone, Debug)]
pub struct OrchestratorClient {
    pub(crate) http: reqwest::Client,
    pub(crate) config: ClientConfig,
    identity: Option<Identity>,
}

impl OrchestratorClient {
    /// Creates a client from `MOA__ORCHESTRATOR__ENDPOINT`,
    /// `RESTATE_INGRESS_URL`, or the local compose default.
    pub fn from_env() -> Result<Self> {
        let endpoint = std::env::var("MOA__ORCHESTRATOR__ENDPOINT")
            .or_else(|_| std::env::var("RESTATE_INGRESS_URL"))
            .unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
        Self::new(endpoint)
    }

    /// Creates a client for an explicit Restate ingress endpoint.
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        Self::with_config(ClientConfig {
            endpoint: endpoint.into(),
            ..ClientConfig::default()
        })
    }

    /// Creates a client from a full configuration object.
    pub fn with_config(mut config: ClientConfig) -> Result<Self> {
        config.endpoint = config.endpoint.trim_end_matches('/').to_string();
        url::Url::parse(&config.endpoint)
            .map_err(|err| Error::InvalidEndpoint(format!("{}: {err}", config.endpoint)))?;
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            .pool_max_idle_per_host(config.max_idle_per_host)
            .build()?;
        Ok(Self {
            http,
            config,
            identity: None,
        })
    }

    /// Returns the configured Restate ingress endpoint.
    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    /// Attaches identity headers to every subsequent client request.
    #[must_use]
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Returns a handle scoped to one Session virtual object key.
    pub fn session(&self, session_id: impl Into<String>) -> SessionHandle<'_> {
        SessionHandle {
            client: self,
            session_id: session_id.into(),
        }
    }

    /// Persists one session metadata row through `SessionStore/create_session`.
    pub async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
        self.post_call("/SessionStore/create_session", &meta).await
    }

    /// Initializes the Session virtual object state for an already-created row.
    pub async fn init_session_vo(&self, session_id: SessionId, meta: SessionMeta) -> Result<()> {
        self.post_void(
            "/SessionStore/init_session_vo",
            &InitSessionVoRequest { session_id, meta },
        )
        .await
    }

    /// Appends one event to the durable session log.
    pub async fn append_event(&self, session_id: SessionId, event: Event) -> Result<u64> {
        self.post_call(
            "/SessionStore/append_event",
            &AppendEventRequest { session_id, event },
        )
        .await
    }

    /// Loads one persisted session metadata row.
    pub async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.post_call("/SessionStore/get_session", &session_id)
            .await
    }

    /// Loads persisted events for one session.
    pub async fn get_events(
        &self,
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        self.post_call(
            "/SessionStore/get_events",
            &GetEventsRequest { session_id, range },
        )
        .await
    }

    /// Lists persisted sessions matching a filter.
    pub async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        self.post_call(
            "/SessionStore/list_sessions",
            &ListSessionsRequest { filter },
        )
        .await
    }

    /// Aggregates workspace spend since a timestamp.
    pub async fn workspace_cost_since(
        &self,
        workspace_id: WorkspaceId,
        since: DateTime<Utc>,
    ) -> Result<u32> {
        self.post_call(
            "/SessionStore/workspace_cost_since",
            &WorkspaceCostSinceRequest {
                workspace_id,
                since,
            },
        )
        .await
    }

    /// Lists registered tool names for a workspace through `ToolExecutor/list_tools`.
    pub async fn tool_names(&self, workspace_id: WorkspaceId) -> Result<Vec<String>> {
        let descriptors: Vec<ToolDescriptor> = self
            .post_call("/ToolExecutor/list_tools", &workspace_id)
            .await?;
        Ok(descriptors.into_iter().map(|tool| tool.name).collect())
    }

    /// Probes an orchestrator health URL and succeeds only on a 2xx status.
    #[instrument(skip(self))]
    pub async fn health_check(&self, health_url: &str) -> Result<()> {
        let resp = apply_identity_headers(self.http.get(health_url), self.identity.as_ref())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::BadStatus { status, body });
        }
        Ok(())
    }

    pub(crate) async fn post_call<Req, Resp>(&self, path: &str, body: &Req) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        self.post_call_with_idempotency(path, body, None).await
    }

    pub(crate) async fn post_empty_call<Resp>(&self, path: &str) -> Result<Resp>
    where
        Resp: serde::de::DeserializeOwned,
    {
        let url = format!("{}{path}", self.config.endpoint);
        let resp = apply_identity_headers(self.http.post(url), self.identity.as_ref())
            .send()
            .await?;
        decode_response(resp).await
    }

    pub(crate) async fn post_void<Req>(&self, path: &str, body: &Req) -> Result<()>
    where
        Req: Serialize + ?Sized,
    {
        let url = format!("{}{path}", self.config.endpoint);
        let resp = apply_identity_headers(self.http.post(url).json(body), self.identity.as_ref())
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(Error::BadStatus { status, body });
        }
        Ok(())
    }

    pub(crate) async fn post_call_with_idempotency<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
        idempotency_key: Option<&str>,
    ) -> Result<Resp>
    where
        Req: Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        let url = format!("{}{path}", self.config.endpoint);
        let mut request =
            apply_identity_headers(self.http.post(url).json(body), self.identity.as_ref());
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        let resp = request.send().await?;
        decode_response(resp).await
    }
}

async fn decode_response<Resp>(resp: reqwest::Response) -> Result<Resp>
where
    Resp: serde::de::DeserializeOwned,
{
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(Error::BadStatus { status, body });
    }
    Ok(serde_json::from_str(&body)?)
}

fn apply_identity_headers(
    request: reqwest::RequestBuilder,
    identity: Option<&Identity>,
) -> reqwest::RequestBuilder {
    let Some(identity) = identity else {
        return request;
    };
    let mut request = request
        .header(
            "x-moa-identity-type",
            identity_type_str(identity.identity_type),
        )
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

fn identity_type_str(identity_type: IdentityType) -> &'static str {
    match identity_type {
        IdentityType::User => "user",
        IdentityType::Agent => "agent",
        IdentityType::Service => "service",
    }
}
