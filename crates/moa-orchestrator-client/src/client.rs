//! Top-level orchestrator HTTP client and configuration.

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_auth_providers::api_keys::{CreateApiKeyRequest, CreateApiKeyResponse, Env, KeyListItem};
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{
    AppendEventRequest, GetEventsRequest, InitSessionVoRequest, ListSessionsRequest,
    ToolDescriptor, WorkspaceCostSinceRequest,
};
use moa_core::{
    Event, EventRange, EventRecord, SessionFilter, SessionId, SessionMeta, SessionSummary,
    WorkspaceId,
};
use serde::{Deserialize, Serialize};
use tracing::instrument;
use uuid::Uuid;

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
#[derive(Clone)]
pub struct OrchestratorClient {
    pub(crate) http: reqwest::Client,
    pub(crate) config: ClientConfig,
    identity: Option<Identity>,
    bearer: Option<String>,
}

impl fmt::Debug for OrchestratorClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OrchestratorClient")
            .field("config", &self.config)
            .field("identity", &self.identity)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
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
            bearer: None,
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

    /// Attaches an authorization bearer token to every subsequent client request.
    #[must_use]
    pub fn with_bearer(mut self, bearer: impl Into<String>) -> Self {
        self.bearer = Some(bearer.into());
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

    /// Create a local API key through `ApiKeys/create`.
    pub async fn api_keys_create(
        &self,
        name: String,
        env: Env,
        description: Option<String>,
        for_agent_id: Option<Uuid>,
    ) -> Result<CreateApiKeyResponse> {
        self.post_call(
            "/ApiKeys/create",
            &CreateApiKeyRequest {
                name,
                description,
                env,
                for_agent_id,
            },
        )
        .await
    }

    /// List active local API keys owned by the caller through `ApiKeys/list`.
    pub async fn api_keys_list(&self) -> Result<Vec<KeyListItem>> {
        self.post_empty_call("/ApiKeys/list").await
    }

    /// Rotate a local API key through `ApiKeys/rotate`.
    pub async fn api_keys_rotate(&self, id: Uuid) -> Result<CreateApiKeyResponse> {
        self.post_call("/ApiKeys/rotate", &id).await
    }

    /// Revoke a local API key through `ApiKeys/revoke`.
    pub async fn api_keys_revoke(&self, id: Uuid) -> Result<()> {
        self.post_void("/ApiKeys/revoke", &id).await
    }

    /// Return the identity seen by the orchestrator through `Whoami/whoami`.
    pub async fn whoami(&self) -> Result<Identity> {
        if self.bearer.is_some() {
            self.get_call("/v1/whoami").await
        } else {
            self.post_empty_call("/Whoami/whoami").await
        }
    }

    /// List pending approvals for the current user.
    pub async fn approvals_list_mine(&self) -> Result<Vec<ApprovalSummary>> {
        if self.bearer.is_some() {
            self.get_call("/v1/approvals").await
        } else {
            self.post_empty_call("/Approvals/list_mine").await
        }
    }

    /// Resolve one pending approval.
    pub async fn approvals_decide(
        &self,
        id: Uuid,
        outcome: String,
        reason: Option<String>,
    ) -> Result<()> {
        let request = DecisionRequest {
            id,
            outcome,
            reason,
        };
        if self.bearer.is_some() {
            self.post_void(
                &format!("/v1/approvals/{id}/decision"),
                &PublicDecisionRequest {
                    outcome: request.outcome,
                    reason: request.reason,
                },
            )
            .await
        } else {
            self.post_void("/Approvals/decide", &request).await
        }
    }

    /// Create an agent template.
    pub async fn agent_templates_create(
        &self,
        request: CreateAgentTemplateRequest,
    ) -> Result<AgentTemplateSummary> {
        if self.bearer.is_some() {
            self.post_call("/v1/agent-templates", &request).await
        } else {
            self.post_call("/AgentTemplates/create", &request).await
        }
    }

    /// List active agent templates visible to the caller.
    pub async fn agent_templates_list(&self) -> Result<Vec<AgentTemplateSummary>> {
        if self.bearer.is_some() {
            self.get_call("/v1/agent-templates").await
        } else {
            self.post_empty_call("/AgentTemplates/list").await
        }
    }

    /// Load one agent template.
    pub async fn agent_templates_get(&self, id: Uuid) -> Result<AgentTemplateSummary> {
        if self.bearer.is_some() {
            self.get_call(&format!("/v1/agent-templates/{id}")).await
        } else {
            self.post_call("/AgentTemplates/get", &id).await
        }
    }

    /// Deactivate one agent template.
    pub async fn agent_templates_deactivate(&self, id: Uuid) -> Result<()> {
        if self.bearer.is_some() {
            self.post_void(
                &format!("/v1/agent-templates/{id}/deactivate"),
                &serde_json::json!({}),
            )
            .await
        } else {
            self.post_void("/AgentTemplates/deactivate", &id).await
        }
    }

    /// Register an agent from a template.
    pub async fn agents_register(&self, request: RegisterAgentRequest) -> Result<AgentSummary> {
        if self.bearer.is_some() {
            self.post_call("/v1/agents", &request).await
        } else {
            self.post_call("/Agents/register", &request).await
        }
    }

    /// List active agents operated by the caller.
    pub async fn agents_list(&self) -> Result<Vec<AgentSummary>> {
        if self.bearer.is_some() {
            self.get_call("/v1/agents").await
        } else {
            self.post_empty_call("/Agents/list").await
        }
    }

    /// Load one agent.
    pub async fn agents_get(&self, id: Uuid) -> Result<AgentSummary> {
        if self.bearer.is_some() {
            self.get_call(&format!("/v1/agents/{id}")).await
        } else {
            self.post_call("/Agents/get", &id).await
        }
    }

    /// Deactivate one agent.
    pub async fn agents_deactivate(&self, id: Uuid) -> Result<()> {
        if self.bearer.is_some() {
            self.post_void(
                &format!("/v1/agents/{id}/deactivate"),
                &serde_json::json!({}),
            )
            .await
        } else {
            self.post_void("/Agents/deactivate", &id).await
        }
    }

    /// Grant an agent the right to act as a user.
    pub async fn agents_grant_can_act_as(&self, agent_id: Uuid, user_id: Uuid) -> Result<()> {
        let request = AgentActAsRequest { agent_id, user_id };
        if self.bearer.is_some() {
            self.post_void(
                &format!("/v1/agents/{agent_id}/can-act-as"),
                &PublicAgentActAsRequest { user_id },
            )
            .await
        } else {
            self.post_void("/Agents/grant_can_act_as", &request).await
        }
    }

    /// Revoke an agent's right to act as a user.
    pub async fn agents_revoke_can_act_as(&self, agent_id: Uuid, user_id: Uuid) -> Result<()> {
        let request = AgentActAsRequest { agent_id, user_id };
        if self.bearer.is_some() {
            self.post_void(
                &format!("/v1/agents/{agent_id}/revoke-can-act-as"),
                &PublicAgentActAsRequest { user_id },
            )
            .await
        } else {
            self.post_void("/Agents/revoke_can_act_as", &request).await
        }
    }

    /// Enqueue one authorization tuple write through the admin helper.
    pub async fn authz_write_tuple(
        &self,
        user: String,
        relation: String,
        object: String,
        tenant_id: Option<Uuid>,
    ) -> Result<()> {
        let request = WriteTupleRequest {
            user,
            relation,
            object,
            tenant_id,
        };
        if self.bearer.is_some() {
            self.post_void("/v1/authz/tuple-write", &request).await
        } else {
            self.post_void("/Authz/write_tuple", &request).await
        }
    }

    /// Ensure a tenant has a signing key.
    pub async fn tenants_ensure_signing_key(&self, tenant_id: Uuid) -> Result<Uuid> {
        self.post_call("/Tenants/ensure_signing_key", &tenant_id)
            .await
    }

    /// Rotate a tenant signing key.
    pub async fn tenants_rotate_signing_key(&self, tenant_id: Uuid) -> Result<Uuid> {
        self.post_call("/Tenants/rotate_signing_key", &tenant_id)
            .await
    }

    /// Set the S3 audit destination for a tenant.
    pub async fn tenants_set_audit_destination(
        &self,
        request: SetAuditDestinationRequest,
    ) -> Result<()> {
        self.post_void("/Tenants/set_audit_destination", &request)
            .await
    }

    /// Verify one OCSF security event signature.
    pub async fn audit_verify(&self, event_id: Uuid) -> Result<AuditVerifyResponse> {
        self.post_call("/Audit/verify", &event_id).await
    }

    /// Probes an orchestrator health URL and succeeds only on a 2xx status.
    #[instrument(skip(self))]
    pub async fn health_check(&self, health_url: &str) -> Result<()> {
        let resp = apply_auth_headers(
            self.http.get(health_url),
            self.identity.as_ref(),
            self.bearer.as_deref(),
        )
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
        let resp = apply_auth_headers(
            self.http.post(url),
            self.identity.as_ref(),
            self.bearer.as_deref(),
        )
        .send()
        .await?;
        decode_response(resp).await
    }

    async fn get_call<Resp>(&self, path: &str) -> Result<Resp>
    where
        Resp: serde::de::DeserializeOwned,
    {
        let url = format!("{}{path}", self.config.endpoint);
        let resp = apply_auth_headers(
            self.http.get(url),
            self.identity.as_ref(),
            self.bearer.as_deref(),
        )
        .send()
        .await?;
        decode_response(resp).await
    }

    pub(crate) async fn post_void<Req>(&self, path: &str, body: &Req) -> Result<()>
    where
        Req: Serialize + ?Sized,
    {
        let url = format!("{}{path}", self.config.endpoint);
        let resp = apply_auth_headers(
            self.http.post(url).json(body),
            self.identity.as_ref(),
            self.bearer.as_deref(),
        )
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
        let mut request = apply_auth_headers(
            self.http.post(url).json(body),
            self.identity.as_ref(),
            self.bearer.as_deref(),
        );
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

fn apply_auth_headers(
    request: reqwest::RequestBuilder,
    identity: Option<&Identity>,
    bearer: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = if let Some(bearer) = bearer {
        request.bearer_auth(bearer)
    } else {
        request
    };
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

/// Pending approval summary returned by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalSummary {
    /// Approval row id.
    pub id: Uuid,
    /// Session waiting on the decision.
    pub session_id: Uuid,
    /// One-line action summary.
    pub action_summary: String,
    /// Full action details.
    pub action_details: serde_json::Value,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Expiration timestamp.
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
struct DecisionRequest {
    id: Uuid,
    outcome: String,
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PublicDecisionRequest {
    outcome: String,
    reason: Option<String>,
}

/// Request body for creating an agent template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentTemplateRequest {
    /// Tenant-unique template name.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// System instructions used by agents instantiated from this template.
    pub instructions: String,
    /// Tool names this template is allowed to call.
    pub allowed_tools: Vec<String>,
}

/// Agent template summary returned by orchestrator APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTemplateSummary {
    /// Template UUID.
    pub id: Uuid,
    /// Tenant UUID.
    pub tenant_id: Uuid,
    /// Tenant-unique template name.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// System instructions used by agents instantiated from this template.
    pub instructions: String,
    /// Tool names this template is allowed to call.
    pub allowed_tools: Vec<String>,
    /// User who created the template.
    pub created_by_user_id: Uuid,
    /// Template creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Template deactivation timestamp.
    pub deactivated_at: Option<DateTime<Utc>>,
}

/// Request body for registering an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterAgentRequest {
    /// Template to instantiate.
    pub template_id: Uuid,
    /// Human-readable agent display name.
    pub display_name: String,
}

/// Agent summary returned by orchestrator APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    /// Agent UUID.
    pub id: Uuid,
    /// Tenant UUID.
    pub tenant_id: Uuid,
    /// Optional template UUID.
    pub template_id: Option<Uuid>,
    /// User who operates the agent. Deactivation cascades can orphan agents.
    pub operator_user_id: Option<Uuid>,
    /// Human-readable agent display name.
    pub display_name: String,
    /// Lifecycle status.
    pub status: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Deactivation timestamp.
    pub deactivated_at: Option<DateTime<Utc>>,
    /// Optional deactivation reason.
    pub deactivated_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AgentActAsRequest {
    agent_id: Uuid,
    user_id: Uuid,
}

#[derive(Debug, Clone, Serialize)]
struct PublicAgentActAsRequest {
    user_id: Uuid,
}

#[derive(Debug, Clone, Serialize)]
struct WriteTupleRequest {
    user: String,
    relation: String,
    object: String,
    tenant_id: Option<Uuid>,
}

/// Request body for configuring a tenant audit destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAuditDestinationRequest {
    /// Tenant id.
    pub tenant_id: Uuid,
    /// Destination S3 bucket.
    pub bucket_name: String,
    /// AWS region for the bucket.
    pub region: String,
    /// Optional role to assume before writing.
    pub assume_role_arn: Option<String>,
    /// Object key prefix.
    pub key_prefix: Option<String>,
    /// Object Lock retention in days.
    pub object_lock_days: Option<i32>,
    /// Optional KMS key ARN.
    pub encryption_kms_key_arn: Option<String>,
}

/// Response for audit event signature verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditVerifyResponse {
    /// Event id.
    pub event_id: Uuid,
    /// Tenant id.
    pub tenant_id: Uuid,
    /// Whether the stored event signature is valid.
    pub valid: bool,
}

fn identity_type_str(identity_type: IdentityType) -> &'static str {
    match identity_type {
        IdentityType::User => "user",
        IdentityType::Agent => "agent",
        IdentityType::Service => "service",
    }
}
