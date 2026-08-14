//! Tool dispatch entry points and single-attempt execution paths.

use std::sync::Arc;
use std::time::Instant;

use moa_connectors::executor::{
    ConnectorActionInvocation, ConnectorInvocationCompletionTicket, SecuredConnectorOutputMetadata,
};
use moa_core::{
    error::MoaError,
    error::Result,
    traits::Identity,
    types::completion::ToolInvocation,
    types::hands::HandHandle,
    types::hands::HandStatus,
    types::identifiers::{
        ExecutionRunScopeId, HandProvisioningOperationId, SandboxWorkspaceId, ToolCallId,
    },
    types::resource::DeadlineGuard,
    types::sandbox_workspace::{ExecutionHandReleaseOwner, SandboxWorkspaceScope, WorkspaceEffect},
    types::security::ToolCapabilityId,
    types::session::SessionMeta,
    types::tools::SecuredToolOutput,
    types::tools::ToolDefinition,
    types::tools::ToolOutput,
    types::worker::state::WorkerInputTarget,
};
use moa_observability::current_turn_root_span;
use moa_security::{OutputClassification, classify_tool_output};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::adapters::mcp::MCPClient;

use super::lifecycle::hand_id;
use super::policy::validate_tool_invocation;
use super::registration::{McpClientRoute, McpRouteGeneration};
use super::telemetry::{
    record_tool_execution_result, record_tool_invocation_metadata, tool_execution_span,
};
use super::{
    DEFAULT_PROVIDER_NAME, HandRoute, ToolCallScope, ToolCatalogSnapshot, ToolExecution, ToolRouter,
};

/// One authorized tool call and all of the identity that must remain stable
/// across preparation, catalog selection, execution, retry, and cancellation.
///
/// `catalog` is `None` when the dispatch should use the router's currently
/// activated snapshot. A caller that already admitted a scoped catalog passes
/// `Some` so the exact publication used for authorization and execution stays
/// attached to the call. The request is intentionally a Rust-only carrier;
/// it is not a wire or persistence payload.
#[derive(Clone, Copy)]
pub struct AuthorizedToolCall<'a> {
    /// Session whose tenant and call origin govern the dispatch.
    pub session: &'a SessionMeta,
    /// Authenticated caller whose delegated rights govern the dispatch.
    pub caller_identity: &'a Identity,
    /// Typed durable workspace owner, absent only for non-sandbox tools.
    ///
    /// A sandbox route rejects `None` before workspace reads or provider I/O;
    /// bare coordinator/session workspace ownership is intentionally
    /// unrepresentable.
    pub workspace_scope: Option<&'a SandboxWorkspaceScope>,
    /// Tool invocation being dispatched.
    pub invocation: &'a ToolInvocation,
    /// Replay-stable durable tool-call identity.
    pub tool_call_id: ToolCallId,
    /// Per-turn canary used by output classification.
    pub active_canary: Option<&'a str>,
    /// Exact catalog publication selected by the caller, when already known.
    pub catalog: Option<&'a ToolCatalogSnapshot>,
    /// Cancellation and resource budget governing every dispatch stage.
    pub scope: ToolCallScope<'a>,
}

/// Journal-safe result of a command step whose workspace publication is separate.
///
/// The secured output can be recorded by the durable caller before it starts the
/// checkpoint step. `workspace_commit_required` is true only when a sandbox
/// command actually returned and its declared workspace effect is
/// [`WorkspaceEffect::MayWrite`]; recovery-created failures never request a
/// checkpoint.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredWorkspaceToolOutput {
    /// Classified, budgeted output safe to store in the durable runtime journal.
    pub output: SecuredToolOutput,
    /// Whether the caller must durably publish the workspace before returning output.
    pub workspace_commit_required: bool,
}

impl std::ops::Deref for DeferredWorkspaceToolOutput {
    type Target = SecuredToolOutput;

    fn deref(&self) -> &Self::Target {
        &self.output
    }
}

/// Commit-only continuation for a mutable sandbox result already journaled by the caller.
///
/// It deliberately carries no caller-supplied provider, workspace, hand, or
/// catalog identifiers. The router resolves those from the verified session and
/// typed scope, while `tool_call_id` selects the deterministic persisted commit.
#[derive(Clone, Copy)]
pub struct JournaledWorkspaceCommit<'a> {
    /// Session whose tenant owns the durable workspace.
    pub session: &'a SessionMeta,
    /// Typed durable owner journaled with the command step.
    pub workspace_scope: &'a SandboxWorkspaceScope,
    /// Replay-stable tool call that owns the deterministic commit operation.
    pub tool_call_id: ToolCallId,
    /// Fresh bounded budget owned by the durable commit step.
    pub scope: ToolCallScope<'a>,
}

/// One idempotent request to release an execution attempt's exact sandbox hand.
#[derive(Clone, Copy)]
pub struct ExecutionHandReleaseRequest<'a> {
    /// Session whose tenant owns the execution workspace and hand lease.
    pub session: &'a SessionMeta,
    /// Verified durable execution run.
    pub run_id: ExecutionRunScopeId,
    /// Verified durable execution owner and logical generation.
    pub owner: ExecutionHandReleaseOwner,
    /// Exact bounded attempt generation yielding its resources.
    pub attempt_generation: u64,
    /// Fresh bounded budget for checkpoint publication and verified destroy.
    pub scope: ToolCallScope<'a>,
}

/// One idempotent request to park a worker's sandbox for a human-input wait.
#[derive(Clone, Copy)]
pub struct WorkerHandReleaseRequest<'a> {
    /// Session whose tenant owns the worker workspace and hand lease.
    pub session: &'a SessionMeta,
    /// Exact worker scope entering the durable wait.
    pub worker_id: &'a str,
    /// Turn, admission generation, and input request that own this wait.
    pub input_target: &'a WorkerInputTarget,
    /// Exact compute attachment captured before the input wait was registered.
    pub expected: Option<&'a WorkerHandReleaseFence>,
    /// Fresh bounded budget for checkpoint publication and verified destroy.
    pub scope: ToolCallScope<'a>,
}

/// Exact durable workspace and lease generation a worker is allowed to park.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerHandReleaseFence {
    /// Workspace attached to the worker when the wait began.
    pub workspace_id: SandboxWorkspaceId,
    /// Single-writer epoch that owned the working copy.
    pub writer_epoch: u64,
    /// Compute instance generation that owned the working copy.
    pub instance_generation: u64,
    /// Provider route pinned on the workspace.
    pub provider: String,
    /// Provisioning operation that created the exact compute instance.
    pub provisioning_operation_id: HandProvisioningOperationId,
    /// Durable hand-lease generation attached to the workspace.
    pub hand_lease_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkspaceCommitMode {
    Inline,
    Deferred,
}

impl<'a> AuthorizedToolCall<'a> {
    /// Attaches the exact catalog publication resolved by a dispatch wrapper.
    fn with_catalog(self, catalog: &'a ToolCatalogSnapshot) -> Self {
        Self {
            catalog: Some(catalog),
            ..self
        }
    }
}

/// Everything one MCP dispatch needs beyond the authorized call and its definition.
///
/// The durable tool-call identity is carried by [`AuthorizedToolCall`] and is
/// reused across retries and sent to the server.
#[derive(Clone)]
pub(super) struct McpDispatch<'a> {
    /// Configured MCP server that owns the remote tool.
    pub(super) server_name: &'a str,
    /// Tool name as the owning server knows it.
    ///
    /// Registered tool names are server-qualified, so the qualified name would
    /// be rejected by the server. Carrying the remote name explicitly is what
    /// keeps qualification a local concern instead of something every connector
    /// has to be taught about.
    pub(super) remote_tool_name: &'a str,
    /// Client route published in the same catalog snapshot as the tool schema.
    pub(super) client_route: McpClientRoute,
    /// Generation observed by the most recent initialization attempt.
    ///
    /// Zero is the generation of a route that has not installed a client yet.
    /// Recovery passes this value back to reconnection so a failure from an old
    /// client cannot replace a newer one that won the race.
    pub(super) expected_generation: McpRouteGeneration,
}

/// Classified connector output awaiting durable journaling and completion.
///
/// The raw upstream response has already passed through the single hands output
/// classifier. The secret-free ticket remains paired with that secured result
/// so an in-process durable caller can journal both, then finalize the connector
/// invocation through `moa-connectors`. Generic router dispatch never produces
/// this type and therefore can never retry a connector action.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingConnectorToolOutput {
    secured_output: SecuredToolOutput,
    secured_metadata: SecuredConnectorOutputMetadata,
    completion_ticket: ConnectorInvocationCompletionTicket,
}

impl PendingConnectorToolOutput {
    /// Returns the classified output that the durable caller must journal.
    #[must_use]
    pub const fn secured_output(&self) -> &SecuredToolOutput {
        &self.secured_output
    }

    /// Returns secret-free classifier metadata for post-journal finalization.
    #[must_use]
    pub const fn secured_metadata(&self) -> &SecuredConnectorOutputMetadata {
        &self.secured_metadata
    }

    /// Returns the private-constructor completion ticket to finalize after journaling.
    #[must_use]
    pub const fn completion_ticket(&self) -> &ConnectorInvocationCompletionTicket {
        &self.completion_ticket
    }

    /// Consumes the pending result into its durable, secret-free components.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        SecuredToolOutput,
        SecuredConnectorOutputMetadata,
        ConnectorInvocationCompletionTicket,
    ) {
        (
            self.secured_output,
            self.secured_metadata,
            self.completion_ticket,
        )
    }
}

impl ToolRouter {
    /// Classifies one raw provider return, then budgets the *safe* output.
    ///
    /// Every raw-output source funnels through here, and the order is the whole
    /// point: classification runs first, so output budgeting, artifactization,
    /// telemetry, and every downstream persistence path only ever see bytes the
    /// detector has already redacted or destroyed. Artifactizing first would
    /// write raw malicious bytes into durable blob storage that nothing later
    /// re-reads through the classifier.
    async fn secure_and_budget(
        &self,
        session: &SessionMeta,
        tool_definition: &ToolDefinition,
        capability: ToolCapabilityId,
        active_canary: Option<&str>,
        raw: ToolOutput,
        hand_id: Option<String>,
    ) -> SecuredToolOutput {
        let secured = classify_tool_output(
            &raw,
            OutputClassification {
                capability: &capability,
                active_canary,
            },
        );
        let safe_output = self
            .apply_output_budget(session, tool_definition, secured.safe_output)
            .await;
        SecuredToolOutput {
            safe_output,
            assessment: secured.assessment,
            capability,
            hand_id,
        }
    }

    /// Classifies a router-created failure output that never reached a provider.
    ///
    /// Recovery-created error text is still output the model will read, so it is
    /// classified on the same path rather than trusted because MOA wrote it.
    pub(super) fn secure_router_output(
        capability: ToolCapabilityId,
        active_canary: Option<&str>,
        raw: ToolOutput,
        hand_id: Option<String>,
    ) -> SecuredToolOutput {
        classify_tool_output(
            &raw,
            OutputClassification {
                capability: &capability,
                active_canary,
            },
        )
        .with_hand_id(hand_id)
    }

    /// Executes one already-authorized tool invocation.
    ///
    /// The scope is admitted before resolving the activated catalog, so a
    /// cancelled or exhausted call does not perform a catalog lookup. A caller
    /// that already owns a scoped publication stores it in `request.catalog`;
    /// otherwise this method captures the router's active publication after
    /// admission.
    pub async fn execute_authorized(
        &self,
        request: AuthorizedToolCall<'_>,
    ) -> Result<SecuredToolOutput> {
        request.scope.admit()?;
        match request.catalog {
            Some(catalog) => {
                self.execute_authorized_from_catalog(request.with_catalog(catalog))
                    .await
            }
            None => {
                let catalog = self.activated_catalog();
                self.execute_authorized_from_catalog(request.with_catalog(&catalog))
                    .await
            }
        }
    }

    async fn execute_authorized_from_catalog(
        &self,
        request: AuthorizedToolCall<'_>,
    ) -> Result<SecuredToolOutput> {
        let Some(catalog) = request.catalog else {
            return Err(MoaError::ConfigError(
                "authorized tool call is missing its catalog selection".to_string(),
            ));
        };
        let tool_span = tool_execution_span(request.session, request.invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            // Asked first, before policy preparation and before the registry
            // lookup: a scope that is already dead must not provision a sandbox,
            // open an MCP connection, or run a built-in, and every one of those
            // happens below this line.
            request.scope.admit()?;
            let started_at = Instant::now();
            let prepared = self
                .prepare_invocation_from_catalog(catalog, request.session, request.invocation)
                .await?;
            let registry = &catalog.registry;
            let registered_tool = registry
                .tools
                .get(&request.invocation.name)
                .ok_or_else(|| registry.unknown_tool_error(&request.invocation.name))?;
            record_tool_invocation_metadata(
                &tool_span,
                request.session,
                &registered_tool.execution,
                &prepared.policy().effect,
            );
            let result = self.execute_authorized_inner(request).await;
            record_tool_execution_result(
                &tool_span,
                &request.invocation.name,
                started_at.elapsed(),
                &result,
            );
            result
        }
        .instrument(instrument_tool_span)
        .await
    }

    /// Executes one installed connector action exactly once and returns its
    /// classified output plus secret-free completion authority.
    ///
    /// The caller must have completed action-policy review before entering this
    /// method. The connector runtime independently reauthorizes delegated `Use`
    /// and re-reads every durable pin before credentials or network. The caller
    /// must journal the returned secured output and metadata before asking the
    /// connector completion service to consume the ticket.
    /// Executes one installed connector action against its selected catalog.
    ///
    /// Scope admission happens before resolving the active catalog and before
    /// the connector runtime can read credentials or perform network I/O.
    pub async fn execute_installed_connector_pending(
        &self,
        request: AuthorizedToolCall<'_>,
    ) -> Result<PendingConnectorToolOutput> {
        request.scope.admit()?;
        match request.catalog {
            Some(catalog) => {
                self.execute_installed_connector_pending_from_catalog(request.with_catalog(catalog))
                    .await
            }
            None => {
                let catalog = self.activated_catalog();
                self.execute_installed_connector_pending_from_catalog(
                    request.with_catalog(&catalog),
                )
                .await
            }
        }
    }

    async fn execute_installed_connector_pending_from_catalog(
        &self,
        request: AuthorizedToolCall<'_>,
    ) -> Result<PendingConnectorToolOutput> {
        let Some(catalog) = request.catalog else {
            return Err(MoaError::ConfigError(
                "authorized connector call is missing its catalog selection".to_string(),
            ));
        };
        self.require_owned_catalog(catalog)?;
        request.scope.admit()?;
        let registry = &catalog.registry;
        let registered_tool = registry
            .tools
            .get(&request.invocation.name)
            .ok_or_else(|| registry.unknown_tool_error(&request.invocation.name))?;
        validate_tool_invocation(&registered_tool.definition, request.invocation)?;
        let ToolExecution::InstalledConnectorAction {
            runtime, prepared, ..
        } = &registered_tool.execution
        else {
            return Err(MoaError::ValidationError(format!(
                "tool `{}` is not an installed connector action",
                request.invocation.name
            )));
        };
        let action = registered_tool
            .execution
            .installed_connector_pin()
            .ok_or_else(|| {
                MoaError::ValidationError(
                    "installed connector execution is missing its typed action pin".to_string(),
                )
            })?;
        let capability = registered_tool
            .execution
            .capability_id(&request.invocation.name);
        moa_security::admit_capability_for_origin(
            self.effective_call_origin(request.session),
            &capability,
            registered_tool.definition.policy.action_class,
        )?;

        let tool_span = tool_execution_span(request.session, request.invocation);
        record_tool_invocation_metadata(
            &tool_span,
            request.session,
            &registered_tool.execution,
            &registered_tool.definition.policy.default_effect,
        );
        let started_at = Instant::now();
        let cancellation_token = request
            .scope
            .effective_cancel_token()
            .cloned()
            .unwrap_or_else(CancellationToken::new);
        let raw_result = self
            .run_within_scope(
                request.scope,
                async {
                    runtime
                        .invoke(
                            ConnectorActionInvocation {
                                caller: request.caller_identity.clone(),
                                tool_call_id: request.tool_call_id,
                                action,
                                input: request.invocation.input.clone(),
                                cancellation_token,
                            },
                            prepared.as_ref().clone(),
                        )
                        .await
                        .map_err(connector_runtime_error)
                }
                .instrument(tool_span.clone()),
            )
            .await?;
        let (raw_output, completion_ticket) = raw_result.into_parts();
        let secured_output = self
            .secure_and_budget(
                request.session,
                &registered_tool.definition,
                capability,
                request.active_canary,
                ToolOutput::json(
                    "Connector action completed.",
                    raw_output,
                    started_at.elapsed(),
                ),
                None,
            )
            .await;
        let secured_output_bytes = serde_json::to_vec(&secured_output.safe_output)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?
            .len() as u64;
        let pending = PendingConnectorToolOutput {
            secured_metadata: SecuredConnectorOutputMetadata {
                assessment: secured_output.assessment.clone(),
                secured_output_bytes,
            },
            secured_output,
            completion_ticket,
        };
        let telemetry_result = Ok(pending.secured_output.clone());
        record_tool_execution_result(
            &tool_span,
            &request.invocation.name,
            started_at.elapsed(),
            &telemetry_result,
        );
        Ok(pending)
    }

    /// Executes an already-authorized tool invocation with retry and recovery enabled.
    ///
    /// `request.workspace_scope` selects the typed worker or execution-task
    /// owner whose hand may be provisioned or reused. An absent scope is valid
    /// only for non-sandbox tools and is rejected before sandbox lifecycle work.
    pub async fn execute_authorized_with_recovery(
        &self,
        request: AuthorizedToolCall<'_>,
    ) -> Result<SecuredToolOutput> {
        request.scope.admit()?;
        match request.catalog {
            Some(catalog) => {
                self.execute_authorized_with_recovery_from_catalog(request.with_catalog(catalog))
                    .await
            }
            None => {
                let catalog = self.activated_catalog();
                self.execute_authorized_with_recovery_from_catalog(request.with_catalog(&catalog))
                    .await
            }
        }
    }

    async fn execute_authorized_with_recovery_from_catalog(
        &self,
        request: AuthorizedToolCall<'_>,
    ) -> Result<SecuredToolOutput> {
        self.execute_authorized_with_recovery_from_catalog_mode(
            request,
            WorkspaceCommitMode::Inline,
        )
        .await
        .map(|result| result.output)
    }

    /// Executes the command step while deferring any mutable-workspace publication.
    ///
    /// Durable orchestrators must journal the returned secured output, run
    /// [`ToolRouter::commit_authorized_workspace_after_tool`] as a second durable
    /// step when requested, and expose the output only after that step succeeds.
    pub async fn execute_authorized_with_recovery_deferred_workspace_commit(
        &self,
        request: AuthorizedToolCall<'_>,
    ) -> Result<DeferredWorkspaceToolOutput> {
        request.scope.admit()?;
        match request.catalog {
            Some(catalog) => {
                self.execute_authorized_with_recovery_from_catalog_mode(
                    request.with_catalog(catalog),
                    WorkspaceCommitMode::Deferred,
                )
                .await
            }
            None => {
                let catalog = self.activated_catalog();
                self.execute_authorized_with_recovery_from_catalog_mode(
                    request.with_catalog(&catalog),
                    WorkspaceCommitMode::Deferred,
                )
                .await
            }
        }
    }

    async fn execute_authorized_with_recovery_from_catalog_mode(
        &self,
        request: AuthorizedToolCall<'_>,
        workspace_commit_mode: WorkspaceCommitMode,
    ) -> Result<DeferredWorkspaceToolOutput> {
        let Some(catalog) = request.catalog else {
            return Err(MoaError::ConfigError(
                "authorized recovery call is missing its catalog selection".to_string(),
            ));
        };
        request.scope.admit()?;
        self.require_owned_catalog(catalog)?;
        let tool_span = tool_execution_span(request.session, request.invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            request.scope.admit()?;
            let started_at = Instant::now();
            let registry = &catalog.registry;
            let registered_tool = registry
                .tools
                .get(&request.invocation.name)
                .ok_or_else(|| registry.unknown_tool_error(&request.invocation.name))?;
            validate_tool_invocation(&registered_tool.definition, request.invocation)?;
            // The durable path deliberately does not re-run action policy: the
            // caller cleared it before enqueuing the call. Origin admission is
            // not part of that clearance — it is a property of the runtime this
            // router serves and of the session it serves it for, not of the
            // decision the caller made — so it is enforced here too. Without it,
            // an experiment trial would reach every production capability simply
            // by taking the recovery path, which is the path every orchestrated
            // tool call takes.
            moa_security::admit_capability_for_origin(
                self.effective_call_origin(request.session),
                &registered_tool
                    .execution
                    .capability_id(&request.invocation.name),
                registered_tool.definition.policy.action_class,
            )?;
            record_tool_invocation_metadata(
                &tool_span,
                request.session,
                &registered_tool.execution,
                &moa_core::types::action_policy::ActionPolicyEffect::Allow,
            );
            let result = self
                .execute_authorized_with_recovery_inner(request, workspace_commit_mode)
                .await;
            record_tool_execution_result(
                &tool_span,
                &request.invocation.name,
                started_at.elapsed(),
                &result,
            );
            result
        }
        .instrument(instrument_tool_span)
        .await
    }

    async fn execute_authorized_inner(
        &self,
        request: AuthorizedToolCall<'_>,
    ) -> Result<SecuredToolOutput> {
        let Some(catalog) = request.catalog else {
            return Err(MoaError::ConfigError(
                "authorized tool call is missing its catalog selection".to_string(),
            ));
        };
        let registry = &catalog.registry;
        let registered_tool = registry
            .tools
            .get(&request.invocation.name)
            .ok_or_else(|| registry.unknown_tool_error(&request.invocation.name))?;
        let capability = registered_tool
            .execution
            .capability_id(&request.invocation.name);
        match &registered_tool.execution {
            ToolExecution::BuiltIn(tool) => {
                self.execute_builtin_once(&request, &registered_tool.definition, &capability, tool)
                    .await
            }
            ToolExecution::Hand { routes } => {
                if request.workspace_scope.is_none() {
                    return Err(MoaError::PermissionDenied(
                        "sandbox tools require a typed worker or execution-task workspace scope"
                            .to_string(),
                    ));
                }
                let route = primary_hand_route(routes)?;
                self.execute_hand_once(&request, &registered_tool.definition, &capability, route)
                    .await
            }
            ToolExecution::Mcp {
                server_name,
                remote_tool_name,
                ..
            } => {
                let client_route = registered_tool.mcp_client_route.clone().ok_or_else(|| {
                    MoaError::ConfigError(format!(
                        "MCP tool {} has no client route in its catalog snapshot",
                        request.invocation.name
                    ))
                })?;
                let mut dispatch = McpDispatch {
                    server_name,
                    remote_tool_name,
                    client_route,
                    expected_generation: 0,
                };
                self.execute_mcp_once_with_scope(
                    &request,
                    &registered_tool.definition,
                    &capability,
                    &mut dispatch,
                )
                .await
            }
            ToolExecution::InstalledConnectorAction { .. } => Err(MoaError::ToolError(
                "installed connector actions require the durable pending-output dispatch path"
                    .to_string(),
            )),
        }
    }

    pub(super) async fn execute_builtin_once(
        &self,
        request: &AuthorizedToolCall<'_>,
        tool_definition: &ToolDefinition,
        capability: &ToolCapabilityId,
        tool: &Arc<dyn moa_core::traits::BuiltInTool>,
    ) -> Result<SecuredToolOutput> {
        let memory_tool_executor = self.bindings.memory_tool_executor();
        let memory_retrieval_executor =
            self.bindings.memory_retrieval_executor.read().await.clone();
        let lineage = self.bindings.lineage();
        let session_store = self.bindings.session_store();
        let tool_call_id = request.tool_call_id.to_string();
        let ctx = moa_core::traits::ToolContext {
            session: request.session,
            caller_identity: request.caller_identity,
            tool_call_id: Some(&tool_call_id),
            lineage: lineage.as_ref(),
            session_store: session_store.as_deref(),
            cancel_token: request.scope.cancel_token,
            budget: request.scope.budget,
            memory_tool_executor: memory_tool_executor.as_deref(),
            memory_retrieval_executor: memory_retrieval_executor.as_deref(),
        };
        // Built-ins are in-process, so the scope's own deadline is a real
        // enforcement point: expiry cancels the shared token the tool is holding
        // rather than only dropping this future.
        let output = self
            .run_within_scope(request.scope, tool.execute(&request.invocation.input, &ctx))
            .await?;
        Ok(self
            .secure_and_budget(
                request.session,
                tool_definition,
                capability.clone(),
                request.active_canary,
                output,
                None,
            )
            .await)
    }

    /// Runs `future` bounded by one caller scope.
    ///
    /// The scope's token is used directly rather than a child: the budget's
    /// deadline is the *run's* deadline, so when it expires everything under
    /// that run must stop, not just the future this call happens to hold. That
    /// is the difference between propagating a deadline and merely observing
    /// one.
    pub(super) async fn run_within_scope<F, T>(
        &self,
        scope: ToolCallScope<'_>,
        future: F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        if scope.budget.deadline.is_none() && scope.effective_cancel_token().is_none() {
            return future.await;
        }
        let token = scope
            .effective_cancel_token()
            .cloned()
            .unwrap_or_else(CancellationToken::new);
        DeadlineGuard::from_budget(token, scope.budget)
            .run(future)
            .await?
    }

    pub(super) async fn execute_hand_once(
        &self,
        request: &AuthorizedToolCall<'_>,
        tool_definition: &ToolDefinition,
        capability: &ToolCapabilityId,
        route: &HandRoute,
    ) -> Result<SecuredToolOutput> {
        // Provisioning is itself paid work — a cold container, a remote
        // workspace — so the scope is re-checked here rather than only at the
        // dispatch entry point, which may have admitted seconds ago.
        request.scope.admit()?;
        let workspace_scope = request.workspace_scope.ok_or_else(|| {
            MoaError::PermissionDenied("sandbox tools require a typed workspace scope".to_string())
        })?;
        let hand = self
            .get_or_provision_hand_within(route, request.session, workspace_scope, request.scope)
            .await?;
        self.execute_hand_on_handle(
            request,
            tool_definition,
            capability,
            &route.provider,
            &hand,
            WorkspaceCommitMode::Inline,
        )
        .await
        .map(|result| result.output)
    }

    /// Executes a tool on an already-provisioned hand handle.
    ///
    /// The recovery path provisions and health-checks the hand once and passes it
    /// here, so it does not re-provision per attempt.
    pub(super) async fn execute_hand_on_handle(
        &self,
        request: &AuthorizedToolCall<'_>,
        tool_definition: &ToolDefinition,
        capability: &ToolCapabilityId,
        provider: &str,
        hand: &HandHandle,
        workspace_commit_mode: WorkspaceCommitMode,
    ) -> Result<DeferredWorkspaceToolOutput> {
        request.scope.admit()?;
        let provider_impl =
            self.hands.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
        let status = self
            .run_within_scope(request.scope, provider_impl.status(hand))
            .await?;
        if matches!(status, HandStatus::Paused) {
            self.run_within_scope(request.scope, provider_impl.resume(hand))
                .await?;
        }
        let workspace_scope = request.workspace_scope.ok_or_else(|| {
            MoaError::PermissionDenied("sandbox tools require a typed workspace scope".to_string())
        })?;
        let lease_scope = super::lifecycle::workspace_lease_scope(workspace_scope);
        self.install_trusted_files_for_hand(
            request.session,
            Some(lease_scope.as_str()),
            provider,
            hand,
            request.scope,
        )
        .await?;

        let serialized_input = serde_json::to_string(&request.invocation.input)?;
        let output = if provider == DEFAULT_PROVIDER_NAME {
            let local_provider = self.hands.local_provider().ok_or_else(|| {
                MoaError::ProviderError("local provider missing from tool router".to_string())
            })?;
            local_provider
                .execute_bounded(
                    hand,
                    &request.invocation.name,
                    &serialized_input,
                    request.scope.effective_cancel_token(),
                    request.scope.budget,
                )
                .await?
        } else {
            // Remote sandbox providers get the budget so they can push a
            // deadline into the execution target; the scope still bounds the
            // call from this side, so a provider that ignores the budget cannot
            // outlive the run either way.
            self.run_within_scope(
                request.scope,
                provider_impl.execute_within(
                    hand,
                    &request.invocation.name,
                    &serialized_input,
                    request.scope.budget,
                ),
            )
            .await?
        };

        let workspace_effect = request
            .catalog
            .and_then(|catalog| catalog.registry.workspace_effect(&request.invocation.name))
            .ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "sandbox tool {} has no declared workspace effect",
                    request.invocation.name
                ))
            })?;
        if workspace_effect == WorkspaceEffect::MayWrite
            && workspace_commit_mode == WorkspaceCommitMode::Inline
            && let Err(error) = self
                .commit_workspace_after_tool(
                    super::sandbox_workspace::lifecycle::WorkspaceCommitExecution {
                        session: request.session,
                        workspace_scope,
                        tool_call_id: request.tool_call_id,
                        provider_name: provider,
                        hand,
                        call_scope: request.scope,
                        release_compute: false,
                    },
                )
                .await
        {
            return Err(match error {
                MoaError::Cancelled | MoaError::BudgetExhausted(_) => error,
                MoaError::ExternalEffectUnknownOutcome { .. } => error,
                _ => MoaError::ExternalEffectUnknownOutcome {
                    operation_id: format!("workspace-tool-call:{}", request.tool_call_id),
                },
            });
        }

        let output = self
            .secure_and_budget(
                request.session,
                tool_definition,
                capability.clone(),
                request.active_canary,
                output,
                Some(hand_id(hand)),
            )
            .await;
        Ok(DeferredWorkspaceToolOutput {
            output,
            workspace_commit_required: workspace_effect == WorkspaceEffect::MayWrite
                && workspace_commit_mode == WorkspaceCommitMode::Deferred,
        })
    }

    pub(super) async fn execute_mcp_once_with_scope(
        &self,
        request: &AuthorizedToolCall<'_>,
        tool_definition: &ToolDefinition,
        capability: &ToolCapabilityId,
        dispatch: &mut McpDispatch<'_>,
    ) -> Result<SecuredToolOutput> {
        const MCP_DISPATCH_METHOD: &str = "tools/call";
        let server_name = dispatch.server_name;
        let cancel_token = request.scope.effective_cancel_token();
        let span = mcp_dispatch_span(server_name, MCP_DISPATCH_METHOD);
        let record_span = span.clone();
        async move {
            request.scope.admit()?;
            let started_at = Instant::now();
            let server = self.mcp.server(server_name).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown MCP server: {server_name}"))
            })?;
            // Connecting performs the up-front discovery probe on a cold MCP
            // dispatch, so the scope bounds discovery as well as the call.
            let (client, generation) = self
                .run_within_scope(
                    request.scope,
                    self.mcp_client(server_name, &dispatch.client_route),
                )
                .await?;
            dispatch.expected_generation = generation;
            // Data-class egress governance: before the payload leaves the trust
            // boundary, classify the serialized tool arguments against this
            // server's `allowed_data_classes` allowlist. Fails closed — a
            // disallowed class or a classification error is a permission denial
            // and the tool is never called. Constructor validation guarantees a
            // guard for every configured MCP server; keep the dispatch check
            // fail-closed as defense in depth for manually assembled routers.
            let guard = self.mcp.egress_guard().ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "MCP server '{}' has no required egress guard",
                    server.name
                ))
            })?;
            let outbound_payload = serde_json::to_string(&request.invocation.input)?;
            self.run_within_scope(request.scope, async {
                guard
                    .check(
                        &server.name,
                        &server.allowed_data_classes,
                        &outbound_payload,
                    )
                    .await
                    .map_err(MoaError::from)
            })
            .await?;
            let tool_invocation_id = request.tool_call_id.to_string();
            let output = self
                .run_within_scope(
                    request.scope,
                    client.call_tool(
                        dispatch.remote_tool_name,
                        request.invocation.input.clone(),
                        Some(&tool_invocation_id),
                        cancel_token,
                    ),
                )
                .await?;
            record_span.record(
                "moa.mcp.latency_ms",
                started_at.elapsed().as_millis() as i64,
            );
            Ok(self
                .secure_and_budget(
                    request.session,
                    tool_definition,
                    capability.clone(),
                    request.active_canary,
                    output,
                    None,
                )
                .await)
        }
        .instrument(span)
        .await
    }

    /// Returns this server's transport client, connecting on first use.
    ///
    /// Connections are opened lazily rather than held from startup, so a
    /// configured connector that is never invoked never costs a socket or a
    /// discovery probe, and a connector whose transport died between refreshes is
    /// reconnected by the call that needs it instead of failing until the next
    /// catalog refresh.
    pub(super) async fn mcp_client(
        &self,
        server_name: &str,
        client_route: &McpClientRoute,
    ) -> Result<(Arc<MCPClient>, McpRouteGeneration)> {
        let server = self
            .mcp
            .server(server_name)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown MCP server: {server_name}")))?;
        let headers = self.mcp.headers_for(&server)?;
        let mut route = client_route.lock().await;
        if let Some(client) = route.client.as_ref() {
            return Ok((Arc::clone(client), route.generation));
        }
        // The route mutex deliberately covers one cold discovery probe. That makes
        // all callers for this route observe the same installed client, while
        // the guard is dropped before this function returns and before any
        // remote tool request begins.
        let client = Arc::new(MCPClient::connect(&server, headers).await?);
        let generation = route.install(Arc::clone(&client))?;
        Ok((client, generation))
    }

    pub(super) async fn reconnect_mcp_client(
        &self,
        server_name: &str,
        client_route: &McpClientRoute,
        expected_generation: McpRouteGeneration,
        scope: ToolCallScope<'_>,
    ) -> Result<bool> {
        let server = self
            .mcp
            .server(server_name)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown MCP server: {server_name}")))?;
        let headers = self.mcp.headers_for(&server)?;
        let mut route = self
            .run_within_scope(scope, async { Ok(client_route.lock().await) })
            .await?;
        // The route mutex deliberately remains held through connect/install for
        // single-flight replacement, but a waiter may have expired while the
        // winning discovery probe held it. Re-admit before treating that waiter as a
        // harmless stale generation.
        scope.admit()?;
        if route.generation != expected_generation {
            return Ok(false);
        }
        let client = Arc::new(
            self.run_within_scope(scope, MCPClient::connect(&server, headers))
                .await?,
        );
        route.install(client)?;
        Ok(true)
    }
}

fn connector_runtime_error(error: moa_connectors::Error) -> MoaError {
    match error {
        moa_connectors::Error::AuthorizationDenied => {
            MoaError::PermissionDenied("connector use authorization denied".to_string())
        }
        moa_connectors::Error::AuthorizationUnavailable => {
            MoaError::PermissionDenied("connector use authorization unavailable".to_string())
        }
        moa_connectors::Error::Cancelled { .. } => MoaError::Cancelled,
        moa_connectors::Error::ManualReconciliationRequired { invocation_id } => {
            MoaError::ExternalEffectUnknownOutcome {
                operation_id: invocation_id.to_string(),
            }
        }
        other => MoaError::ToolError(other.to_string()),
    }
}

/// Builds an MCP dispatch span parented to the active turn root when present.
///
/// `server` and `method` are configuration-bounded values.
fn mcp_dispatch_span(server: &str, method: &'static str) -> tracing::Span {
    match current_turn_root_span() {
        Some(parent) => tracing::info_span!(
            parent: &parent,
            "mcp_dispatch",
            moa.mcp.server = %server,
            moa.mcp.method = method,
            moa.mcp.latency_ms = tracing::field::Empty,
        ),
        None => tracing::info_span!(
            "mcp_dispatch",
            moa.mcp.server = %server,
            moa.mcp.method = method,
            moa.mcp.latency_ms = tracing::field::Empty,
        ),
    }
}

pub(super) fn primary_hand_route(routes: &[HandRoute]) -> Result<&HandRoute> {
    routes
        .first()
        .ok_or_else(|| MoaError::ConfigError("hand tool has no configured provider route".into()))
}

#[cfg(test)]
mod egress_dispatch_tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::{ToolCallScope, connector_runtime_error};
    use async_trait::async_trait;

    use moa_config::McpServerConfig;
    use moa_core::types::security::{SensitivityClass, ToolCapabilityId};
    use moa_core::{
        traits::{BuiltInTool, Identity, ToolContext},
        types::action_policy::ActionClass,
        types::action_policy::ActionPolicyEffect,
        types::action_policy::RiskLevel,
        types::completion::ToolInvocation,
        types::identifiers::SessionId,
        types::identifiers::TenantId,
        types::identifiers::ToolCallId,
        types::session::SessionMeta,
        types::tools::IdempotencyClass,
        types::tools::ToolDefinition,
        types::tools::ToolDiffStrategy,
        types::tools::ToolInputShape,
        types::tools::ToolOutput,
        types::tools::ToolPolicySpec,
    };
    use moa_memory_pii::{MockClassifier, PiiResult};
    use moa_security::McpEgressGuard;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};
    use tokio_util::sync::CancellationToken;

    use uuid::Uuid;

    use crate::core::{ToolRegistry, ToolRouter};

    use super::McpDispatch;

    const SERVER_NAME: &str = "external-search";
    const MCP_TOOL_INVOCATION_ID: &str = "00000000-0000-0000-0000-000000000f01";

    #[test]
    fn connector_unknown_outcome_is_not_erased_to_generic_tool_error() {
        // Pins: the execution-only caller must be able to distinguish a durable
        // ambiguous effect from an ordinary tool failure without inspecting text.
        let mapped = connector_runtime_error(moa_connectors::Error::ManualReconciliationRequired {
            invocation_id: moa_connectors::domain::ConnectorInvocationId::from(Uuid::from_u128(
                0xabc,
            )),
        });

        assert!(
            !matches!(mapped, moa_core::error::MoaError::ToolError(_)),
            "manual-reconciliation provenance was erased at the hands boundary"
        );
    }

    /// Spawns a fake MCP server that answers stateless discovery and records
    /// whether a `tools/call` request ever arrives. Returns the server URL and a
    /// flag set to `true` the moment an outbound tool call reaches the server, so
    /// a test can assert the underlying `call_tool` was (or was not) invoked.
    async fn spawn_recording_mcp_server() -> (String, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake MCP server");
        let addr = listener.local_addr().expect("fake MCP server address");
        let tools_call_seen = Arc::new(AtomicBool::new(false));
        let seen = tools_call_seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = vec![0_u8; 8192];
                let bytes = match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => continue,
                    Ok(read) => read,
                };
                let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                let request_json = request
                    .split_once("\r\n\r\n")
                    .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok());
                let method = request_json
                    .as_ref()
                    .and_then(|value| value.get("method"))
                    .and_then(|method| method.as_str());
                let body = match method {
                    Some("server/discover") => r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"moa-test-server","version":"1.0.0"}},"ttlMs":60000,"cacheScope":"private"}}"#.to_string(),
                    Some("tools/call") => {
                        let invocation_id = request_json
                            .as_ref()
                            .and_then(|value| value.get("params"))
                            .and_then(|params| params.get("_meta"))
                            .and_then(|metadata| metadata.get("moa/toolInvocationId"))
                            .and_then(serde_json::Value::as_str);
                        if invocation_id == Some(MCP_TOOL_INVOCATION_ID) {
                            seen.store(true, Ordering::SeqCst);
                            r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"pong"}],"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"moa-test-server","version":"1.0.0"}}}}"#.to_string()
                        } else {
                            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"tools/call _meta.moa/toolInvocationId must equal the durable tool-call ID"}}"#.to_string()
                        }
                    }
                    // Any method outside this fixture's discovery/call surface gets an empty ack.
                    _ => "{}".to_string(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), tools_call_seen)
    }

    #[derive(Clone, Copy)]
    enum TestMcpServerMode {
        CountOnly,
        GateDiscovery,
        GateSecondDiscovery,
        BlockToolCall,
        FailToolCall,
    }

    struct TestMcpServer {
        url: String,
        discovery_count: Arc<AtomicUsize>,
        discovery_seen: Option<oneshot::Receiver<()>>,
        call_seen: Option<oneshot::Receiver<()>>,
        release: Option<oneshot::Sender<()>>,
        shutdown: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl TestMcpServer {
        async fn shutdown(mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            self.task
                .await
                .expect("test MCP server should stop cleanly");
        }
    }

    async fn spawn_test_mcp_server(mode: TestMcpServerMode) -> TestMcpServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test MCP server");
        let addr = listener.local_addr().expect("read test MCP server address");
        let (discovery_seen_tx, discovery_seen_rx) = oneshot::channel();
        let (call_seen_tx, call_seen_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let discovery_count = Arc::new(AtomicUsize::new(0));
        let observed_count = Arc::clone(&discovery_count);
        let task = tokio::spawn(async move {
            let mut discovery_seen_tx = Some(discovery_seen_tx);
            let mut call_seen_tx = Some(call_seen_tx);
            let mut release_rx = Some(release_rx);
            loop {
                let accepted = tokio::select! {
                    _ = &mut shutdown_rx => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _)) = accepted else {
                    return;
                };
                let mut buffer = vec![0_u8; 8192];
                let Ok(bytes) = socket.read(&mut buffer).await else {
                    continue;
                };
                let request = String::from_utf8_lossy(&buffer[..bytes]);
                let request_json = request
                    .split_once("\r\n\r\n")
                    .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok());
                let method = request_json
                    .as_ref()
                    .and_then(|value| value.get("method"))
                    .and_then(serde_json::Value::as_str);
                let body = match method {
                    Some("server/discover") => {
                        let discovery_number = observed_count.fetch_add(1, Ordering::SeqCst) + 1;
                        if matches!(mode, TestMcpServerMode::GateDiscovery)
                            || matches!(mode, TestMcpServerMode::GateSecondDiscovery)
                                && discovery_number == 2
                        {
                            if let Some(seen) = discovery_seen_tx.take() {
                                let _ = seen.send(());
                            }
                            if let Some(release) = release_rx.take() {
                                let _ = release.await;
                            }
                        }
                        r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"moa-test-server","version":"1.0.0"}},"ttlMs":60000,"cacheScope":"private"}}"#
                    }
                    Some("tools/call") => {
                        if matches!(mode, TestMcpServerMode::BlockToolCall) {
                            if let Some(seen) = call_seen_tx.take() {
                                let _ = seen.send(());
                            }
                            if let Some(release) = release_rx.take() {
                                let _ = release.await;
                            }
                        }
                        if matches!(mode, TestMcpServerMode::FailToolCall) {
                            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"remote failure"}}"#
                        } else {
                            r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","content":[{"type":"text","text":"pong"}],"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"moa-test-server","version":"1.0.0"}}}}"#
                        }
                    }
                    _ => "{}",
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        TestMcpServer {
            url: format!("http://{addr}"),
            discovery_count,
            discovery_seen: matches!(
                mode,
                TestMcpServerMode::GateDiscovery | TestMcpServerMode::GateSecondDiscovery
            )
            .then_some(discovery_seen_rx),
            call_seen: (matches!(mode, TestMcpServerMode::BlockToolCall)).then_some(call_seen_rx),
            release: matches!(
                mode,
                TestMcpServerMode::GateDiscovery
                    | TestMcpServerMode::GateSecondDiscovery
                    | TestMcpServerMode::BlockToolCall
            )
            .then_some(release_tx),
            shutdown: Some(shutdown_tx),
            task,
        }
    }

    /// Builds a guard whose classifier reports every payload as `restricted`.
    fn restricted_class_guard() -> Arc<McpEgressGuard> {
        let classifier = Arc::new(MockClassifier {
            fixed: PiiResult {
                class: SensitivityClass::Restricted,
                spans: Vec::new(),
                model_version: "test-mock".to_string(),
                abstained: false,
            },
        });
        Arc::new(McpEgressGuard::new(classifier))
    }

    /// Builds a router wired to `server` with the fake client already connected,
    /// optionally carrying an egress guard.
    fn router_with_mcp_server(
        server: McpServerConfig,
        guard: Option<Arc<McpEgressGuard>>,
    ) -> ToolRouter {
        let mut router = ToolRouter::new(
            ToolRegistry::default_local(),
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );
        router.mcp.servers.insert(server.name.clone(), server);
        router.mcp.set_egress_guard(guard);
        router
    }

    fn router_with_registered_mcp_server(
        server: McpServerConfig,
        guard: Arc<McpEgressGuard>,
    ) -> ToolRouter {
        let mut registry = ToolRegistry::new();
        let registered_name = registry
            .register_mcp_tool(
                SERVER_NAME,
                crate::adapters::mcp::McpDiscoveredTool {
                    name: "external_tool".to_string(),
                    description: "external MCP tool".to_string(),
                    input_schema: json!({"type": "object"}),
                },
            )
            .expect("register test MCP tool");
        assert_eq!(registered_name, qualified_tool_name());
        let mut router = ToolRouter::new(
            registry,
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );
        router.mcp.servers.insert(server.name.clone(), server);
        router.mcp.set_egress_guard(Some(guard));
        router
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            ..SessionMeta::default()
        }
    }

    fn external_tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "external_tool".to_string(),
            description: "external MCP tool".to_string(),
            schema: json!({ "type": "object" }),
            policy: ToolPolicySpec {
                risk_level: RiskLevel::Low,
                default_effect: ActionPolicyEffect::Allow,
                action_class: ActionClass::DataExport,
                input_shape: ToolInputShape::Json,
                diff_strategy: ToolDiffStrategy::None,
            },
            idempotency_class: IdempotencyClass::NonIdempotent,
            async_mode: moa_core::types::tools::ToolAsyncMode::SynchronousOnly,
            rollback: None,
            max_output_tokens: 4096,
        }
    }

    /// The server-qualified reference the discovered tool registers under.
    fn qualified_tool_name() -> String {
        crate::core::mcp_tool_reference(SERVER_NAME, "external_tool")
    }

    fn tool_invocation() -> ToolInvocation {
        ToolInvocation {
            id: None,
            name: qualified_tool_name(),
            input: json!({ "note": "patient record" }),
        }
    }

    fn http_server(url: String, allowed: Vec<SensitivityClass>) -> McpServerConfig {
        McpServerConfig {
            required: false,
            discovery: moa_config::McpDiscoveryMode::Eager,
            name: SERVER_NAME.to_string(),
            url,
            credentials: None,
            trust_tool_annotations: false,
            allowed_data_classes: allowed,
        }
    }

    fn deployment_dispatch() -> McpDispatch<'static> {
        McpDispatch {
            server_name: SERVER_NAME,
            remote_tool_name: "external_tool",
            client_route: Arc::new(tokio::sync::Mutex::new(
                crate::core::registration::McpClientRouteState::empty(),
            )),
            expected_generation: 0,
        }
    }

    fn identity() -> Identity {
        Identity {
            identity_type: moa_core::traits::IdentityType::Operator,
            id: Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c421),
            tenant_id: moa_core::types::identifiers::TenantId::from(Uuid::from_u128(
                0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c422,
            )),
            api_key_id: None,
            acting_on_behalf_of: None,
        }
    }

    fn authorized_mcp_call<'a>(
        session: &'a SessionMeta,
        caller_identity: &'a Identity,
        invocation: &'a ToolInvocation,
        scope: ToolCallScope<'a>,
    ) -> super::AuthorizedToolCall<'a> {
        super::AuthorizedToolCall {
            session,
            caller_identity,
            workspace_scope: None,
            invocation,
            tool_call_id: ToolCallId(Uuid::from_u128(0x0f01)),
            active_canary: None,
            catalog: None,
            scope,
        }
    }

    struct CapturingToolCallId {
        seen: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl BuiltInTool for CapturingToolCallId {
        fn name(&self) -> &'static str {
            "capture_tool_call_id"
        }

        fn description(&self) -> &'static str {
            "captures the router-provided tool call identity"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object", "additionalProperties": false})
        }

        fn policy_spec(&self) -> ToolPolicySpec {
            ToolPolicySpec {
                risk_level: RiskLevel::Low,
                default_effect: ActionPolicyEffect::Allow,
                action_class: ActionClass::Read,
                input_shape: ToolInputShape::Json,
                diff_strategy: ToolDiffStrategy::None,
            }
        }

        fn idempotency_class(&self) -> IdempotencyClass {
            IdempotencyClass::Idempotent
        }

        async fn execute(
            &self,
            _input: &serde_json::Value,
            ctx: &ToolContext<'_>,
        ) -> moa_core::error::Result<ToolOutput> {
            *self.seen.lock().expect("lock captured tool-call id") =
                ctx.tool_call_id.map(str::to_string);
            Ok(ToolOutput::text("captured", Duration::ZERO))
        }
    }

    #[tokio::test]
    async fn built_in_uses_authorized_tool_call_id_when_provider_id_is_missing_offline() {
        // Pins: a provider may omit its transcript-level invocation id, but the
        // built-in must still receive MOA's replay-stable AuthorizedToolCall id.
        let seen = Arc::new(Mutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CapturingToolCallId {
            seen: Arc::clone(&seen),
        }));
        let router = ToolRouter::new(
            registry,
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );
        let session = session();
        let caller = identity();
        let invocation = ToolInvocation {
            id: None,
            name: "capture_tool_call_id".to_string(),
            input: json!({}),
        };
        let tool_call_id = ToolCallId(Uuid::from_u128(0xc411));

        let secured = router
            .execute_authorized(super::AuthorizedToolCall {
                session: &session,
                caller_identity: &caller,
                workspace_scope: None,
                invocation: &invocation,
                tool_call_id,
                active_canary: None,
                catalog: None,
                scope: ToolCallScope::unbounded(),
            })
            .await
            .expect("built-in dispatch should not require a provider invocation id");

        assert_eq!(
            seen.lock().expect("read captured tool-call id").as_deref(),
            Some(tool_call_id.to_string().as_str()),
            "the built-in context must carry the authorized durable identity"
        );
        assert_eq!(
            secured.capability,
            ToolCapabilityId::builtin("capture_tool_call_id")
        );
        assert_eq!(secured.safe_output.to_text(), "captured");
    }

    #[tokio::test]
    async fn qualified_mcp_failure_keeps_registry_resolved_capability_offline() {
        // Pins: the qualified registry name is lookup metadata only. A failed
        // remote call must keep the same (server, remote tool) capability used
        // by successful dispatch, or recovery would split one security circuit.
        let server = spawn_test_mcp_server(TestMcpServerMode::FailToolCall).await;
        let router = router_with_registered_mcp_server(
            http_server(server.url.clone(), vec![SensitivityClass::Restricted]),
            restricted_class_guard(),
        );
        let session = session();
        let caller = identity();
        let invocation = tool_invocation();

        let secured = router
            .execute_authorized_with_recovery(super::AuthorizedToolCall {
                session: &session,
                caller_identity: &caller,
                workspace_scope: None,
                invocation: &invocation,
                tool_call_id: ToolCallId(Uuid::from_u128(0xc412)),
                active_canary: None,
                catalog: None,
                scope: ToolCallScope::unbounded(),
            })
            .await
            .expect("MCP recovery should return a classified remote failure");

        assert!(
            secured.safe_output.is_error,
            "remote failure must stay an error"
        );
        assert_eq!(
            secured.capability,
            ToolCapabilityId::mcp(SERVER_NAME, "external_tool"),
            "recovery must not substitute the qualified registry lookup name"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_cold_mcp_calls_share_one_route_client_offline() {
        // Pins: eight cold callers on one route perform exactly one stateless
        // discovery probe and all receive the same client allocation. Replacing this
        // route mutex with the old check-then-connect shape must produce eight
        // discovery requests and fail the exact count and pointer assertions.
        let server = spawn_test_mcp_server(TestMcpServerMode::CountOnly).await;
        let router = Arc::new(router_with_mcp_server(
            http_server(server.url.clone(), Vec::new()),
            None,
        ));
        let route = Arc::new(tokio::sync::Mutex::new(
            crate::core::registration::McpClientRouteState::empty(),
        ));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let router = Arc::clone(&router);
            let route = Arc::clone(&route);
            tasks.push(tokio::spawn(async move {
                router.mcp_client(SERVER_NAME, &route).await
            }));
        }

        let mut clients = Vec::new();
        for task in tasks {
            let (client, generation) = task
                .await
                .expect("cold MCP caller should join")
                .expect("cold MCP connection should succeed");
            assert_eq!(generation, 1, "the first installed client is generation 1");
            clients.push(client);
        }

        assert_eq!(
            server.discovery_count.load(Ordering::SeqCst),
            1,
            "one route must single-flight its cold discovery probe"
        );
        let first = clients.first().expect("at least one cold caller");
        assert!(
            clients.iter().all(|client| Arc::ptr_eq(first, client)),
            "every caller must receive the installed route client"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn independent_mcp_routes_discover_without_sharing_lock_offline() {
        // Pins: a blocked discovery probe on route A must not prevent route B from
        // completing its own cold discovery. A router-wide lock would leave B
        // waiting until A is released and fail the bounded completion check.
        let mut first_server = spawn_test_mcp_server(TestMcpServerMode::GateDiscovery).await;
        let second_server = spawn_test_mcp_server(TestMcpServerMode::CountOnly).await;
        let mut first_config = http_server(first_server.url.clone(), Vec::new());
        first_config.name = "gated".to_string();
        let mut second_config = http_server(second_server.url.clone(), Vec::new());
        second_config.name = "independent".to_string();
        let mut router = router_with_mcp_server(first_config, None);
        router
            .mcp
            .servers
            .insert(second_config.name.clone(), second_config);
        let router = Arc::new(router);
        let first_route = Arc::new(tokio::sync::Mutex::new(
            crate::core::registration::McpClientRouteState::empty(),
        ));
        let second_route = Arc::new(tokio::sync::Mutex::new(
            crate::core::registration::McpClientRouteState::empty(),
        ));

        let first_router = Arc::clone(&router);
        let first_route_for_task = Arc::clone(&first_route);
        let first_task = tokio::spawn(async move {
            first_router
                .mcp_client("gated", &first_route_for_task)
                .await
        });
        first_server
            .discovery_seen
            .take()
            .expect("gated server should expose its discovery barrier")
            .await
            .expect("route A should reach its discovery probe");

        let second_result = timeout(
            Duration::from_secs(1),
            router.mcp_client("independent", &second_route),
        )
        .await;
        let second_completed = matches!(second_result, Ok(Ok((_, 1))));

        let _ = first_server
            .release
            .take()
            .expect("gated server should expose its release")
            .send(());
        let (_, first_generation) = first_task
            .await
            .expect("route A caller should join")
            .expect("route A discovery should succeed");

        assert!(
            second_completed,
            "route B must finish while route A's discovery is blocked"
        );
        assert_eq!(first_generation, 1);
        assert_eq!(
            first_server.discovery_count.load(Ordering::SeqCst),
            1,
            "route A should perform one discovery probe"
        );
        assert_eq!(
            second_server.discovery_count.load(Ordering::SeqCst),
            1,
            "route B should perform one discovery probe"
        );

        first_server.shutdown().await;
        second_server.shutdown().await;
    }

    #[tokio::test]
    async fn stale_mcp_reconnect_cannot_replace_a_newer_route_generation_offline() {
        // Pins: two recovery attempts carrying the same failed generation may
        // install only one replacement. The queued stale attempt must refuse
        // replacement after the first attempt advances the route generation;
        // removing that check creates a third discovery probe and generation 3.
        let server = spawn_test_mcp_server(TestMcpServerMode::CountOnly).await;
        let router = router_with_mcp_server(http_server(server.url.clone(), Vec::new()), None);
        let route = Arc::new(tokio::sync::Mutex::new(
            crate::core::registration::McpClientRouteState::empty(),
        ));
        let (_, initial_generation) = router
            .mcp_client(SERVER_NAME, &route)
            .await
            .expect("initial MCP connection should succeed");
        assert_eq!(initial_generation, 1);

        let (first, second) = tokio::join!(
            router.reconnect_mcp_client(
                SERVER_NAME,
                &route,
                initial_generation,
                ToolCallScope::unbounded(),
            ),
            router.reconnect_mcp_client(
                SERVER_NAME,
                &route,
                initial_generation,
                ToolCallScope::unbounded(),
            ),
        );
        let replaced = [
            first.expect("first reconnect should classify its outcome"),
            second.expect("second reconnect should classify its outcome"),
        ];
        assert_eq!(
            replaced.iter().filter(|replaced| **replaced).count(),
            1,
            "exactly one reconnect may replace generation {initial_generation}"
        );
        assert_eq!(
            replaced.iter().filter(|replaced| !**replaced).count(),
            1,
            "the stale reconnect must be refused after the newer client wins"
        );
        let route = route.lock().await;
        assert_eq!(route.generation, 2);
        assert!(
            route.client.is_some(),
            "the newer client must remain installed"
        );
        assert_eq!(
            server.discovery_count.load(Ordering::SeqCst),
            2,
            "initial discovery plus one replacement must be the exact probe count"
        );
        drop(route);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_mcp_reconnect_waiter_stops_before_discovery_completes_offline() {
        // Pins: a reconnect queued behind the per-route discovery lock still
        // belongs to its caller's scope. Cancellation must stop that waiter
        // before the winning reconnect advances the generation and releases it.
        let mut server = spawn_test_mcp_server(TestMcpServerMode::GateSecondDiscovery).await;
        let router = Arc::new(router_with_mcp_server(
            http_server(server.url.clone(), Vec::new()),
            None,
        ));
        let route = Arc::new(tokio::sync::Mutex::new(
            crate::core::registration::McpClientRouteState::empty(),
        ));
        let (_, initial_generation) = router
            .mcp_client(SERVER_NAME, &route)
            .await
            .expect("initial MCP connection should succeed");
        assert_eq!(initial_generation, 1);

        let discovery_router = Arc::clone(&router);
        let discovery_route = Arc::clone(&route);
        let discovery = tokio::spawn(async move {
            discovery_router
                .reconnect_mcp_client(
                    SERVER_NAME,
                    &discovery_route,
                    initial_generation,
                    ToolCallScope::unbounded(),
                )
                .await
        });
        server
            .discovery_seen
            .take()
            .expect("gated server should expose the reconnect discovery barrier")
            .await
            .expect("winning reconnect should hold the route discovery lock");

        let cancellation = CancellationToken::new();
        let waiter_token = cancellation.clone();
        let waiter_router = Arc::clone(&router);
        let waiter_route = Arc::clone(&route);
        let (waiter_started_tx, waiter_started_rx) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _ = waiter_started_tx.send(());
            waiter_router
                .reconnect_mcp_client(
                    SERVER_NAME,
                    &waiter_route,
                    initial_generation,
                    ToolCallScope::from_tokens(Some(&waiter_token), None),
                )
                .await
        });
        waiter_started_rx
            .await
            .expect("contending reconnect should start");
        tokio::task::yield_now().await;
        cancellation.cancel();

        let waiter_result = timeout(Duration::from_millis(250), waiter)
            .await
            .expect("cancelled reconnect must not wait for the discovery lock")
            .expect("contending reconnect task should join");
        assert!(
            matches!(waiter_result, Err(moa_core::error::MoaError::Cancelled)),
            "cancelled reconnect must return MoaError::Cancelled, got {waiter_result:?}"
        );
        assert!(
            !discovery.is_finished(),
            "the winning reconnect must still hold the discovery barrier"
        );

        let _ = server
            .release
            .take()
            .expect("gated server should expose its release")
            .send(());
        assert!(
            discovery
                .await
                .expect("winning reconnect task should join")
                .expect("winning reconnect should complete"),
            "the winning reconnect should install generation 2"
        );
        assert_eq!(server.discovery_count.load(Ordering::SeqCst), 2);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn mcp_route_lock_is_released_before_remote_tool_execution_offline() {
        // Pins: once discovery returns, a blocked remote tools/call must
        // not hold the route mutex. A second route-state read must complete
        // before the fake server releases the remote call.
        let mut server = spawn_test_mcp_server(TestMcpServerMode::BlockToolCall).await;
        let router = Arc::new(router_with_mcp_server(
            http_server(server.url.clone(), vec![SensitivityClass::Restricted]),
            Some(restricted_class_guard()),
        ));
        let route = Arc::new(tokio::sync::Mutex::new(
            crate::core::registration::McpClientRouteState::empty(),
        ));
        let call_router = Arc::clone(&router);
        let call_route = Arc::clone(&route);
        let call_task = tokio::spawn(async move {
            let session = session();
            let caller = identity();
            let invocation = tool_invocation();
            let definition = external_tool_definition();
            let mut dispatch = McpDispatch {
                server_name: SERVER_NAME,
                remote_tool_name: "external_tool",
                client_route: call_route,
                expected_generation: 0,
            };
            let request =
                authorized_mcp_call(&session, &caller, &invocation, ToolCallScope::unbounded());
            call_router
                .execute_mcp_once_with_scope(
                    &request,
                    &definition,
                    &ToolCapabilityId::mcp(SERVER_NAME, "external_tool"),
                    &mut dispatch,
                )
                .await
        });

        timeout(
            Duration::from_secs(1),
            server
                .call_seen
                .take()
                .expect("blocking server should expose its call barrier"),
        )
        .await
        .expect("the remote call should reach the fake server")
        .expect("the call barrier should send");
        let route_access = timeout(
            Duration::from_millis(100),
            router.mcp_client(SERVER_NAME, &route),
        )
        .await;
        let route_accessed = matches!(route_access, Ok(Ok((_, 1))));

        let _ = server
            .release
            .take()
            .expect("blocking server should expose its release")
            .send(());
        let output = call_task
            .await
            .expect("MCP call task should join")
            .expect("MCP call should succeed after release");
        assert_eq!(output.safe_output.to_text(), "pong");
        assert!(
            route_accessed,
            "route state must remain accessible while remote execution is blocked"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn mcp_egress_guard_blocks_restricted_payload_before_dispatch_offline() {
        // Pins: when the egress guard classifies the outbound arguments as
        // restricted and the destination server does not allowlist that class, the
        // dispatch fails closed with a permission denial naming the server, and the
        // underlying MCP `tools/call` is never sent.
        let (url, tools_call_seen) = spawn_recording_mcp_server().await;
        let router =
            router_with_mcp_server(http_server(url, Vec::new()), Some(restricted_class_guard()));

        let session = session();
        let caller = identity();
        let invocation = tool_invocation();
        let definition = external_tool_definition();
        let mut dispatch = deployment_dispatch();
        let request =
            authorized_mcp_call(&session, &caller, &invocation, ToolCallScope::unbounded());
        let error = router
            .execute_mcp_once_with_scope(
                &request,
                &definition,
                &ToolCapabilityId::mcp(SERVER_NAME, "external_tool"),
                &mut dispatch,
            )
            .await
            .expect_err("restricted payload to a default-allowlist server must be blocked");

        assert!(
            matches!(
                error,
                moa_core::error::MoaError::PermissionDenied(message)
                    if message.contains(SERVER_NAME) && message.contains("restricted")
            ),
            "a blocked dispatch must be a permission denial naming the server and class"
        );
        assert!(
            !tools_call_seen.load(Ordering::SeqCst),
            "a blocked egress check must not invoke the MCP tool call"
        );
    }

    #[tokio::test]
    async fn mcp_egress_guard_allows_when_server_allowlists_class_offline() {
        // Pins: the identical restricted payload dispatches normally once the
        // destination server explicitly allowlists the restricted class.
        let (url, tools_call_seen) = spawn_recording_mcp_server().await;
        let router = router_with_mcp_server(
            http_server(url, vec![SensitivityClass::Restricted]),
            Some(restricted_class_guard()),
        );

        let session = session();
        let caller = identity();
        let invocation = tool_invocation();
        let definition = external_tool_definition();
        let mut dispatch = deployment_dispatch();
        let request =
            authorized_mcp_call(&session, &caller, &invocation, ToolCallScope::unbounded());
        let secured = router
            .execute_mcp_once_with_scope(
                &request,
                &definition,
                &ToolCapabilityId::mcp(SERVER_NAME, "external_tool"),
                &mut dispatch,
            )
            .await
            .expect("an allowlisted class must dispatch to the MCP server");

        assert_eq!(secured.safe_output.to_text(), "pong");
        assert!(
            tools_call_seen.load(Ordering::SeqCst),
            "an allowed egress check must dispatch the MCP tool call"
        );
    }

    #[tokio::test]
    async fn mcp_dispatch_without_required_egress_guard_fails_closed_offline() {
        // Pins: manually assembled routers cannot bypass the required MCP egress
        // check; dispatch fails before the outbound tool call when no guard exists.
        let (url, tools_call_seen) = spawn_recording_mcp_server().await;
        let router = router_with_mcp_server(http_server(url, Vec::new()), None);

        let session = session();
        let caller = identity();
        let invocation = tool_invocation();
        let definition = external_tool_definition();
        let mut dispatch = deployment_dispatch();
        let request =
            authorized_mcp_call(&session, &caller, &invocation, ToolCallScope::unbounded());
        let error = router
            .execute_mcp_once_with_scope(
                &request,
                &definition,
                &ToolCapabilityId::mcp(SERVER_NAME, "external_tool"),
                &mut dispatch,
            )
            .await
            .expect_err("MCP dispatch without a guard must fail closed");

        assert!(
            matches!(
                error,
                moa_core::error::MoaError::ConfigError(message)
                    if message.contains(SERVER_NAME) && message.contains("egress guard")
            ),
            "missing guard must be reported as a configuration error"
        );
        assert!(
            !tools_call_seen.load(Ordering::SeqCst),
            "missing guard must prevent the MCP tool call"
        );
    }
}
