//! Configurable-agent and agent-principal MCP tools.

use moa_wire::agents::{
    AgentActAsRequest, AgentDefinitionListRequest, AgentDefinitionListResponse, AgentDeployRequest,
    AgentDeployResponse, AgentDeploymentListRequest, AgentDeploymentListResponse,
    AgentInstallRequest, AgentInstallResponse, AgentInstallationListRequest,
    AgentInstallationListResponse, AgentSummary, RegisterAgentRequest,
};
use moa_wire::experiments::{
    AgentRevisionCompareRequest, AgentRevisionCompareResponse,
    AgentRevisionSimulationCompareRequest, AgentRevisionSimulationCompareResponse,
    AgentRevisionSimulationRunRequest, AgentRevisionSimulationRunResponse,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::command::ServicePath;
use super::{AgentRevisionVariantInput, EmptyInput, Server, clamp_limit};

const DEFINITIONS_LIST: ServicePath = ServicePath::new("/AgentDefinitions/list_definitions");
const INSTALLATIONS_LIST: ServicePath = ServicePath::new("/AgentDefinitions/list_installations");
const DEFINITION_INSTALL: ServicePath = ServicePath::new("/AgentDefinitions/install");
const DEPLOYMENTS_LIST: ServicePath = ServicePath::new("/AgentDefinitions/list_deployments");
const DEFINITION_DEPLOY: ServicePath = ServicePath::new("/AgentDefinitions/deploy");
const REVISION_COMPARE: ServicePath = ServicePath::new("/Experiments/compare_agent_revisions");
const REVISION_SIMULATE: ServicePath =
    ServicePath::new("/Experiments/run_agent_revision_simulation");
const SIMULATION_COMPARE: ServicePath =
    ServicePath::new("/Experiments/compare_agent_revision_simulation");
const PRINCIPAL_REGISTER: ServicePath = ServicePath::new("/Agents/register");
const PRINCIPALS_LIST: ServicePath = ServicePath::new("/Agents/list");
const PRINCIPAL_GET: ServicePath = ServicePath::new("/Agents/get");
const PRINCIPAL_DEACTIVATE: ServicePath = ServicePath::new("/Agents/deactivate");
const PRINCIPAL_GRANT: ServicePath = ServicePath::new("/Agents/grant_can_act_as");
const PRINCIPAL_REVOKE: ServicePath = ServicePath::new("/Agents/revoke_can_act_as");

/// Build the configurable-agent and agent-principal tool router.
pub(super) fn router() -> rmcp::handler::server::router::tool::ToolRouter<Server> {
    Server::agents_router()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentDefinitionsListInput {
    /// Optional artifact lifecycle status; omit to list `published` definitions.
    status: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentInstallInput {
    /// Exact published agent artifact revision UUID returned by `agent_definitions_list`.
    revision_uid: Uuid,
    /// Optional existing agent principal UUID to bind to the installation.
    agent_id: Option<Uuid>,
    /// Optional tenant-local display-name override for this installation.
    display_name: Option<String>,
    /// Optional human-readable reason recorded on the initial deployment.
    reason: Option<String>,
    /// Optional JSON metadata object owned by the dashboard or tenant administration workflow.
    #[serde(default)]
    metadata: Value,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentDeploymentsInput {
    /// Installation UUID returned by `agent_installations_list`.
    installation_uid: Uuid,
    /// Maximum deployment records to return; defaults in the service and is bounded to 1–200.
    #[schemars(range(min = 1, max = 200))]
    limit: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentDeployInput {
    /// Installation UUID whose active deployment pointer will move.
    installation_uid: Uuid,
    /// Exact published agent revision UUID to deploy.
    revision_uid: Uuid,
    /// Optional human-readable reason recorded on the deployment.
    reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentRevisionCompareInput {
    /// Exact published revision UUID used as the baseline.
    base_revision_uid: Uuid,
    /// Exact published revision UUID being considered.
    new_revision_uid: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentRevisionSimulateInput {
    /// Human-readable simulation run name.
    name: String,
    /// Exact published `experiment_plan` revision UUID defining scenarios, scoring, and budgets.
    plan_revision_uid: Uuid,
    /// Baseline variant used for subsequent delta calculations.
    base: AgentRevisionVariantInput,
    /// One or more candidate variants to compare with the base.
    #[serde(default)]
    candidates: Vec<AgentRevisionVariantInput>,
    /// Optional stable retry key; reuse only when retrying the same logical simulation request.
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentSimulationCompareInput {
    /// Completed simulation run UUID returned by `agent_revision_simulate`.
    run_uid: Uuid,
    /// Variant key from that run to treat as the baseline.
    base_variant_key: String,
    /// Candidate variant keys from that run; omit to compare all available candidates.
    #[serde(default)]
    candidate_variant_keys: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentPrincipalRegisterInput {
    /// Human-readable name shown to tenant operators for the new principal.
    display_name: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentPrincipalIdInput {
    /// Agent principal UUID returned by `agent_principals_list` or registration.
    agent_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentPrincipalActAsInput {
    /// Agent principal UUID receiving or losing delegation.
    agent_id: Uuid,
    /// Exact user UUID the agent may or may no longer act as.
    user_id: Uuid,
}

#[tool_router(router = agents_router)]
impl Server {
    /// List visible published configurable-agent definitions.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_definitions_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentDefinitionsListInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, AgentDefinitionListRequest, AgentDefinitionListResponse>(
            context,
            &input,
            DEFINITIONS_LIST,
            "Listed agent definitions.",
        )
        .await
    }

    /// List configurable-agent installations for the authenticated tenant.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_installations_list(
        &self,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.tenant_command::<_, AgentInstallationListRequest, AgentInstallationListResponse>(
            context,
            &EmptyInput {},
            INSTALLATIONS_LIST,
            "Listed agent installations.",
        )
        .await
    }

    /// Install and initially deploy an exact published configurable-agent revision.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn agent_definition_install(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentInstallInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, AgentInstallRequest, AgentInstallResponse>(
            context,
            &input,
            DEFINITION_INSTALL,
            "Installed agent definition.",
        )
        .await
    }

    /// List deployment history for one configurable-agent installation.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_deployments_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut input): Parameters<AgentDeploymentsInput>,
    ) -> CallToolResult {
        input.limit = clamp_limit(input.limit, 200);
        self.tenant_command::<_, AgentDeploymentListRequest, AgentDeploymentListResponse>(
            context,
            &input,
            DEPLOYMENTS_LIST,
            "Listed agent deployments.",
        )
        .await
    }

    /// Deploy an exact published revision to one configurable-agent installation.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn agent_definition_deploy(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentDeployInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, AgentDeployRequest, AgentDeployResponse>(
            context,
            &input,
            DEFINITION_DEPLOY,
            "Deployed agent revision.",
        )
        .await
    }

    /// Compare two resolved published agent revisions before simulation or deployment.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_revision_compare(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentRevisionCompareInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, AgentRevisionCompareRequest, AgentRevisionCompareResponse>(
            context,
            &input,
            REVISION_COMPARE,
            "Compared agent revisions.",
        )
        .await
    }

    /// Run a published experiment plan across exact agent revision variants.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn agent_revision_simulate(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentRevisionSimulateInput>,
    ) -> CallToolResult {
        self.tenant_command::<
            _,
            AgentRevisionSimulationRunRequest,
            AgentRevisionSimulationRunResponse,
        >(
            context,
            &input,
            REVISION_SIMULATE,
            "Accepted agent revision simulation.",
        )
        .await
    }

    /// Compare variant execution and score deltas inside one agent-revision simulation.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_revision_simulation_compare(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentSimulationCompareInput>,
    ) -> CallToolResult {
        self.tenant_command::<
            _,
            AgentRevisionSimulationCompareRequest,
            AgentRevisionSimulationCompareResponse,
        >(
            context,
            &input,
            SIMULATION_COMPARE,
            "Compared agent revision simulation.",
        )
        .await
    }

    /// Register an agent principal. The owning service requires tenant-admin authority.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn agent_principal_register(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentPrincipalRegisterInput>,
    ) -> CallToolResult {
        let request = RegisterAgentRequest {
            display_name: input.display_name,
        };
        self.command::<_, AgentSummary>(
            context,
            &request,
            PRINCIPAL_REGISTER,
            "Registered agent principal.",
        )
        .await
    }

    /// List active agent principals operated by the caller.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_principals_list(&self, context: RequestContext<RoleServer>) -> CallToolResult {
        self.command_empty::<Vec<AgentSummary>>(
            context,
            PRINCIPALS_LIST,
            "Listed agent principals.",
        )
        .await
    }

    /// Load one agent principal. The owning service checks operator/admin authority for that agent.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_principal_get(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentPrincipalIdInput>,
    ) -> CallToolResult {
        self.command::<_, AgentSummary>(
            context,
            &input.agent_id,
            PRINCIPAL_GET,
            "Loaded agent principal.",
        )
        .await
    }

    /// Deactivate an agent principal and revoke its local credentials and delegation tuples.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_principal_deactivate(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentPrincipalIdInput>,
    ) -> CallToolResult {
        self.command::<_, ()>(
            context,
            &input.agent_id,
            PRINCIPAL_DEACTIVATE,
            "Deactivated agent principal.",
        )
        .await
    }

    /// Grant an agent principal permission to act as a user, preserving service-level authority checks.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_principal_grant_act_as(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentPrincipalActAsInput>,
    ) -> CallToolResult {
        let request = AgentActAsRequest {
            agent_id: input.agent_id,
            user_id: input.user_id,
        };
        self.command::<_, ()>(
            context,
            &request,
            PRINCIPAL_GRANT,
            "Granted agent delegation.",
        )
        .await
    }

    /// Revoke an agent principal's permission to act as a user.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn agent_principal_revoke_act_as(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<AgentPrincipalActAsInput>,
    ) -> CallToolResult {
        let request = AgentActAsRequest {
            agent_id: input.agent_id,
            user_id: input.user_id,
        };
        self.command::<_, ()>(
            context,
            &request,
            PRINCIPAL_REVOKE,
            "Revoked agent delegation.",
        )
        .await
    }
}
