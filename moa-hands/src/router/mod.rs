//! Tool routing, local hand provisioning, and built-in/MCP dispatch for MOA.

mod construction;
mod normalization;
mod policy;
mod registration;
mod telemetry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_core::{
    HandHandle, HandProvider, HandResources, HandSpec, McpServerConfig, MemoryStore, MoaError,
    Result, SandboxTier, SessionMeta, SessionStore, ToolInvocation, ToolOutput, WorkspaceId,
};
use moa_security::{ApprovalRuleStore, MCPCredentialProxy, ToolPolicies};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::local::LocalHandProvider;
use crate::mcp::MCPClient;

pub use policy::PreparedToolInvocation;
pub use registration::{ToolExecution, ToolRegistry};
use telemetry::{
    record_tool_execution_result, record_tool_invocation_metadata, tool_execution_span,
};

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// Routes tool invocations to built-ins, local hands, or MCP backends.
pub struct ToolRouter {
    registry: ToolRegistry,
    memory_store: Arc<dyn MemoryStore>,
    providers: HashMap<String, Arc<dyn HandProvider>>,
    local_provider: Option<Arc<LocalHandProvider>>,
    mcp_clients: HashMap<String, Arc<MCPClient>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    mcp_proxy: Option<Arc<MCPCredentialProxy>>,
    active_hands: RwLock<HashMap<String, HandHandle>>,
    workspace_roots: RwLock<HashMap<WorkspaceId, PathBuf>>,
    policies: ToolPolicies,
    rule_store: Option<Arc<dyn ApprovalRuleStore>>,
    session_store: Option<Arc<dyn SessionStore>>,
    sandbox_root: Option<PathBuf>,
}

impl ToolRouter {
    /// Remembers the filesystem root for a logical workspace id.
    pub async fn remember_workspace_root(
        &self,
        workspace_id: WorkspaceId,
        workspace_root: PathBuf,
    ) {
        self.workspace_roots
            .write()
            .await
            .insert(workspace_id, workspace_root);
    }

    /// Destroys and removes all cached hands associated with the provided session.
    pub async fn destroy_session_hands(&self, session_id: &moa_core::SessionId) {
        let session_prefix = format!("{session_id}:");
        let cached = {
            let mut active_hands = self.active_hands.write().await;
            let keys = active_hands
                .keys()
                .filter(|key| key.starts_with(&session_prefix))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| active_hands.remove(&key).map(|handle| (key, handle)))
                .collect::<Vec<_>>()
        };

        for (key, handle) in cached {
            let provider_name = key
                .strip_prefix(&session_prefix)
                .unwrap_or_default()
                .to_string();
            let handle_id = hand_id(&handle);
            let Some(provider) = self.providers.get(&provider_name) else {
                tracing::warn!(
                    session_id = %session_id,
                    provider = %provider_name,
                    hand_id = %handle_id,
                    "cached hand provider missing during cleanup"
                );
                continue;
            };

            match provider.destroy(&handle).await {
                Ok(()) => {
                    tracing::info!(
                        session_id = %session_id,
                        provider = %provider_name,
                        hand_id = %handle_id,
                        "destroyed cached session hand"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        provider = %provider_name,
                        hand_id = %handle_id,
                        error = %error,
                        "failed to destroy cached session hand"
                    );
                }
            }
        }
    }

    /// Executes a single tool invocation for a session.
    pub async fn execute(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        let tool_span = tool_execution_span(session, invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            let started_at = Instant::now();
            let prepared = self.prepare_invocation(session, invocation).await?;
            let registered_tool =
                self.registry.tools.get(&invocation.name).ok_or_else(|| {
                    MoaError::ToolError(format!("unknown tool: {}", invocation.name))
                })?;
            record_tool_invocation_metadata(
                &tool_span,
                session,
                &registered_tool.execution,
                &prepared.policy().action,
            );
            let result = match &prepared.policy().action {
                moa_core::PolicyAction::Allow => {
                    self.execute_authorized_inner(session, invocation, None, None)
                        .await
                }
                moa_core::PolicyAction::Deny => {
                    tool_span.set_attribute("moa.tool.denied", true);
                    Err(MoaError::PermissionDenied(format!(
                        "tool {} denied by policy",
                        invocation.name
                    )))
                }
                moa_core::PolicyAction::RequireApproval => {
                    Err(MoaError::PermissionDenied(format!(
                        "tool {} requires approval: {}",
                        invocation.name,
                        prepared.input_summary()
                    )))
                }
            };

            record_tool_execution_result(&tool_span, started_at.elapsed(), &result);
            result
        }
        .instrument(instrument_tool_span)
        .await
    }

    /// Executes a tool invocation after approval has already been granted.
    pub async fn execute_authorized(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        self.execute_authorized_with_cancel(session, invocation, None, None)
            .await
    }

    /// Executes a tool invocation after approval has already been granted with cancellation hooks.
    pub async fn execute_authorized_with_cancel(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        cancel_token: Option<&CancellationToken>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let tool_span = tool_execution_span(session, invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            let started_at = Instant::now();
            let prepared = self.prepare_invocation(session, invocation).await?;
            let registered_tool =
                self.registry.tools.get(&invocation.name).ok_or_else(|| {
                    MoaError::ToolError(format!("unknown tool: {}", invocation.name))
                })?;
            record_tool_invocation_metadata(
                &tool_span,
                session,
                &registered_tool.execution,
                &prepared.policy().action,
            );
            let result = self
                .execute_authorized_inner(session, invocation, cancel_token, hard_cancel_token)
                .await;
            record_tool_execution_result(&tool_span, started_at.elapsed(), &result);
            result
        }
        .instrument(instrument_tool_span)
        .await
    }

    async fn execute_authorized_inner(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        cancel_token: Option<&CancellationToken>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let registered_tool = self
            .registry
            .tools
            .get(&invocation.name)
            .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?;

        match &registered_tool.execution {
            ToolExecution::BuiltIn(tool) => {
                let ctx = moa_core::ToolContext {
                    session,
                    memory_store: &*self.memory_store,
                    session_store: self.session_store.as_deref(),
                    cancel_token,
                };
                let output = tool.execute(&invocation.input, &ctx).await?;
                Ok((None, output))
            }
            ToolExecution::Hand { provider, tier } => {
                let hand = self
                    .get_or_provision_hand(provider, tier.clone(), session)
                    .await?;
                let provider_impl = self.providers.get(provider).ok_or_else(|| {
                    MoaError::ProviderError(format!("unknown hand provider: {provider}"))
                })?;
                let status = provider_impl.status(&hand).await?;
                if matches!(
                    status,
                    moa_core::HandStatus::Paused | moa_core::HandStatus::Stopped
                ) {
                    provider_impl.resume(&hand).await?;
                }
                let serialized_input = serde_json::to_string(&invocation.input)?;
                let output = if provider == DEFAULT_PROVIDER_NAME {
                    let local_provider = self.local_provider.as_ref().ok_or_else(|| {
                        MoaError::ProviderError(
                            "local provider missing from tool router".to_string(),
                        )
                    })?;
                    local_provider
                        .execute_with_cancel(
                            &hand,
                            &invocation.name,
                            &serialized_input,
                            hard_cancel_token,
                        )
                        .await?
                } else if let Some(hard_cancel_token) = hard_cancel_token {
                    tokio::select! {
                        result = provider_impl.execute(&hand, &invocation.name, &serialized_input) => result?,
                        _ = hard_cancel_token.cancelled() => return Err(MoaError::Cancelled),
                    }
                } else {
                    provider_impl
                        .execute(&hand, &invocation.name, &serialized_input)
                        .await?
                };
                Ok((Some(hand_id(&hand)), output))
            }
            ToolExecution::Mcp { server_name } => {
                let server = self.mcp_servers.get(server_name).ok_or_else(|| {
                    MoaError::ProviderError(format!("unknown MCP server: {server_name}"))
                })?;
                let client = self.mcp_clients.get(server_name).ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "missing MCP client for configured server: {server_name}"
                    ))
                })?;
                let extra_headers = if let (Some(proxy), Some(_credentials)) =
                    (&self.mcp_proxy, server.credentials.as_ref())
                {
                    let token = proxy
                        .create_session_token(&session.id, server_name, server_name)
                        .await?;
                    let headers = proxy
                        .enrich_headers(&token, server.credentials.as_ref())
                        .await?;
                    proxy.revoke_session_token(&token).await;
                    headers
                } else {
                    HashMap::new()
                };
                let output = client
                    .call_tool(&invocation.name, invocation.input.clone(), extra_headers)
                    .await?;
                Ok((None, output))
            }
        }
    }

    async fn get_or_provision_hand(
        &self,
        provider: &str,
        tier: SandboxTier,
        session: &SessionMeta,
    ) -> Result<HandHandle> {
        let key = session_provider_key(session, provider);
        if let Some(handle) = self.active_hands.read().await.get(&key) {
            return Ok(handle.clone());
        }

        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        let workspace_mount =
            if provider == DEFAULT_PROVIDER_NAME && matches!(tier, SandboxTier::Local) {
                self.workspace_roots
                    .read()
                    .await
                    .get(&session.workspace_id)
                    .cloned()
            } else {
                None
            };
        let handle = provider_impl
            .provision(HandSpec {
                sandbox_tier: tier,
                image: None,
                resources: HandResources::default(),
                env: HashMap::new(),
                workspace_mount,
                idle_timeout: DEFAULT_TOOL_TIMEOUT,
                max_lifetime: DEFAULT_TOOL_TIMEOUT,
            })
            .await?;

        self.active_hands.write().await.insert(key, handle.clone());
        Ok(handle)
    }
}

fn session_provider_key(session: &SessionMeta, provider: &str) -> String {
    format!("{}:{provider}", session.id)
}

fn hand_id(handle: &HandHandle) -> String {
    match handle {
        HandHandle::Local { sandbox_dir } => sandbox_dir.display().to_string(),
        HandHandle::Docker { container_id } => container_id.clone(),
        HandHandle::Daytona { workspace_id } => workspace_id.clone(),
        HandHandle::E2B { sandbox_id } => sandbox_id.clone(),
    }
}
