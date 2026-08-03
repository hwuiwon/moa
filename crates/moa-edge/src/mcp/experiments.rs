//! Behavior Lab experiment MCP tools.

use moa_wire::experiments::{
    ExperimentCancelRequest, ExperimentCancelResponse, ExperimentCompareRequest,
    ExperimentCompareResponse, ExperimentGeneratePlanRequest, ExperimentGeneratePlanResponse,
    ExperimentListRequest, ExperimentListResponse, ExperimentProposeImprovementsRequest,
    ExperimentProposeImprovementsResponse, ExperimentRunRequest, ExperimentRunResponse,
    ExperimentRunStatusRequest, ExperimentRunStatusResponse, ExperimentScoresRequest,
    ExperimentScoresResponse, ExperimentTrialStatusRequest, ExperimentTrialStatusResponse,
    ExperimentTrialsRequest, ExperimentTrialsResponse,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::command::ServicePath;
use super::{AgentRevisionVariantInput, Server, clamp_limit};

const EXPERIMENT_GENERATE_PLAN: ServicePath = ServicePath::new("/Experiments/generate_plan");
const EXPERIMENT_LIST: ServicePath = ServicePath::new("/Experiments/list");
const EXPERIMENT_RUN: ServicePath = ServicePath::new("/Experiments/run");
const EXPERIMENT_STATUS: ServicePath = ServicePath::new("/Experiments/status");
const EXPERIMENT_TRIALS: ServicePath = ServicePath::new("/Experiments/trials");
const EXPERIMENT_TRIAL_STATUS: ServicePath = ServicePath::new("/Experiments/trial_status");
const EXPERIMENT_CANCEL: ServicePath = ServicePath::new("/Experiments/cancel");
const EXPERIMENT_SCORES: ServicePath = ServicePath::new("/Experiments/scores");
const EXPERIMENT_COMPARE: ServicePath = ServicePath::new("/Experiments/compare");
const EXPERIMENT_PROPOSE: ServicePath = ServicePath::new("/Experiments/propose_improvements");

/// Build the Behavior Lab experiment tool router.
pub(super) fn router() -> rmcp::handler::server::router::tool::ToolRouter<Server> {
    Server::experiments_router()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentGeneratePlanInput {
    /// Natural-language description of the behavior to test.
    description: String,
    /// Optional model ID override for plan generation; omit to use the tenant default.
    model: Option<String>,
    /// Exact artifact references such as `agent://support@3` or `skill://triage@2` that the plan may use.
    #[serde(default)]
    artifact_refs: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentListInput {
    /// Optional run status: `accepted`, `running`, `completed`, `failed`, or `cancelled`.
    status: Option<String>,
    /// Maximum run summaries to return; defaults in the service and is bounded to 1–200.
    #[schemars(range(min = 1, max = 200))]
    limit: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentRunInput {
    /// Human-readable experiment run name.
    name: String,
    /// Exact published `experiment_plan` revision UUID to execute.
    plan_revision_uid: Uuid,
    /// Optional existing score-run UUID used to join external or previously collected scores.
    score_run_id: Option<Uuid>,
    /// Optional stable retry key; reuse only when retrying the same logical experiment admission.
    idempotency_key: Option<String>,
    /// Exact published agent revision variants for agent-loop experiments.
    #[serde(default)]
    agent_revision_variants: Vec<AgentRevisionVariantInput>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentRunIdInput {
    /// Experiment run UUID returned by `experiment_run` or `agent_revision_simulate`.
    run_uid: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentTrialsInput {
    /// Experiment run UUID whose trials should be listed.
    run_uid: Uuid,
    /// Optional exact trial lifecycle status returned by the owning experiment service.
    status: Option<String>,
    /// Maximum trial summaries to return; defaults in the service and is bounded to 1–200.
    #[schemars(range(min = 1, max = 200))]
    limit: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentTrialInput {
    /// Trial UUID returned by `experiment_trials_list`.
    trial_uid: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentCancelInput {
    /// Active experiment run UUID to cancel.
    run_uid: Uuid,
    /// Optional human-readable cancellation reason stored with the run.
    reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentCompareInput {
    /// Completed experiment run UUID used as the baseline.
    base_run_uid: Uuid,
    /// Completed experiment run UUID containing the candidate behavior.
    new_run_uid: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct ExperimentProposeInput {
    /// Completed experiment run UUID whose evidence should produce proposals.
    run_uid: Uuid,
    /// Optional stable retry key; reuse only when retrying proposal generation for the same evidence.
    idempotency_key: Option<String>,
}

#[tool_router(router = experiments_router)]
impl Server {
    /// Generate and store a draft experiment_plan artifact; validate and publish it separately.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn experiment_plan_generate(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExperimentGeneratePlanInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExperimentGeneratePlanRequest, ExperimentGeneratePlanResponse>(
            context,
            &input,
            EXPERIMENT_GENERATE_PLAN,
            "Generated draft experiment plan.",
        )
        .await
    }

    /// List bounded experiment run summaries.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn experiments_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut input): Parameters<ExperimentListInput>,
    ) -> CallToolResult {
        input.limit = clamp_limit(input.limit, 200);
        self.tenant_command::<_, ExperimentListRequest, ExperimentListResponse>(
            context,
            &input,
            EXPERIMENT_LIST,
            "Listed experiment runs.",
        )
        .await
    }

    /// Admit a live behavior experiment and return its run ID for polling.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn experiment_run(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExperimentRunInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExperimentRunRequest, ExperimentRunResponse>(
            context,
            &input,
            EXPERIMENT_RUN,
            "Accepted experiment run.",
        )
        .await
    }

    /// Poll one experiment run.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn experiment_status(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExperimentRunIdInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExperimentRunStatusRequest, ExperimentRunStatusResponse>(
            context,
            &input,
            EXPERIMENT_STATUS,
            "Loaded experiment status.",
        )
        .await
    }

    /// List bounded trials under one experiment run.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn experiment_trials_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut input): Parameters<ExperimentTrialsInput>,
    ) -> CallToolResult {
        input.limit = clamp_limit(input.limit, 200);
        self.tenant_command::<_, ExperimentTrialsRequest, ExperimentTrialsResponse>(
            context,
            &input,
            EXPERIMENT_TRIALS,
            "Listed experiment trials.",
        )
        .await
    }

    /// Load one experiment trial status.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn experiment_trial_status(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExperimentTrialInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExperimentTrialStatusRequest, ExperimentTrialStatusResponse>(
            context,
            &input,
            EXPERIMENT_TRIAL_STATUS,
            "Loaded experiment trial status.",
        )
        .await
    }

    /// Request cancellation of one active experiment run.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn experiment_cancel(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExperimentCancelInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExperimentCancelRequest, ExperimentCancelResponse>(
            context,
            &input,
            EXPERIMENT_CANCEL,
            "Requested experiment cancellation.",
        )
        .await
    }

    /// Read score summaries for one experiment run.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn experiment_scores(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExperimentRunIdInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExperimentScoresRequest, ExperimentScoresResponse>(
            context,
            &input,
            EXPERIMENT_SCORES,
            "Loaded experiment scores.",
        )
        .await
    }

    /// Compare scores, scenarios, and variants for two experiment runs.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn experiment_compare(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExperimentCompareInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, ExperimentCompareRequest, ExperimentCompareResponse>(
            context,
            &input,
            EXPERIMENT_COMPARE,
            "Compared experiment runs.",
        )
        .await
    }

    /// Propose reviewable learning candidates from completed experiment evidence.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn experiment_propose_improvements(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<ExperimentProposeInput>,
    ) -> CallToolResult {
        self.tenant_command::<
            _,
            ExperimentProposeImprovementsRequest,
            ExperimentProposeImprovementsResponse,
        >(
            context,
            &input,
            EXPERIMENT_PROPOSE,
            "Proposed experiment-backed improvements for review.",
        )
        .await
    }
}
