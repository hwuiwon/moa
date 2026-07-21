//! Internal tenant-operator eval MCP tools.

use moa_wire::eval::{
    EvalCompareRequest, EvalCompareResponse, EvalDatasetListRequest, EvalDatasetListResponse,
    EvalDatasetRegisterRequest, EvalDatasetRegisterResponse, EvalPlanRequest, EvalPlanResponse,
    EvalRunRequest, EvalRunResponse, EvalRunStatusRequest, EvalRunStatusResponse,
    EvalScoresRequest, EvalScoresResponse, EvalSuiteListDocument, EvalSuiteListRequest,
    EvalSuiteListResponse,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::command::ServicePath;
use super::{EmptyInput, Server};

const EVAL_SUITES_LIST: ServicePath = ServicePath::new("/Eval/suites_list");
const EVAL_PLAN: ServicePath = ServicePath::new("/Eval/plan");
const EVAL_DATASETS_LIST: ServicePath = ServicePath::new("/Eval/datasets_list");
const EVAL_DATASET_REGISTER: ServicePath = ServicePath::new("/Eval/datasets_register");
const EVAL_RUN: ServicePath = ServicePath::new("/Eval/run");
const EVAL_RUN_STATUS: ServicePath = ServicePath::new("/Eval/run_status");
const EVAL_SCORES: ServicePath = ServicePath::new("/Eval/scores");
const EVAL_COMPARE: ServicePath = ServicePath::new("/Eval/compare");

/// Build the internal eval tool router.
pub(super) fn router() -> rmcp::handler::server::router::tool::ToolRouter<Server> {
    Server::evals_router()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct EvalSuiteDocumentInput {
    /// Logical suite source path or URI.
    source: Option<String>,
    /// Raw suite TOML document.
    body: String,
}

impl From<EvalSuiteDocumentInput> for EvalSuiteListDocument {
    fn from(value: EvalSuiteDocumentInput) -> Self {
        Self {
            source: value.source,
            body: value.body,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct EvalSuitesInput {
    /// Caller-supplied inline TOML suite documents to summarize.
    #[serde(default)]
    documents: Vec<EvalSuiteDocumentInput>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct EvalPlanInput {
    /// Complete inline TOML eval suite document; do not pass a filesystem path.
    suite_document: String,
    /// Optional logical path or URI used only for diagnostics and provenance.
    suite_source: Option<String>,
    /// Complete inline agent configuration documents referenced by the suite.
    #[serde(default)]
    config_documents: Vec<String>,
    /// Logical path or URI for each config document, in the same order as `config_documents`.
    #[serde(default)]
    config_sources: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct EvalDatasetRegisterInput {
    /// Stable tenant-local dataset name referenced by eval suites.
    name: String,
    /// Complete JSONL content with exactly one JSON object per non-empty line.
    jsonl: String,
    /// Optional source URI recorded for provenance; content is never fetched from this URI.
    source_uri: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct EvalRunInput {
    /// Complete inline TOML eval suite document previously checked with `eval_plan`.
    suite_document: String,
    /// Optional logical path or URI used only for diagnostics and provenance.
    suite_source: Option<String>,
    /// Complete inline agent configuration documents referenced by the suite.
    #[serde(default)]
    config_documents: Vec<String>,
    /// Logical path or URI for each config document, in the same order as `config_documents`.
    #[serde(default)]
    config_sources: Vec<String>,
    /// Report sink specs such as `terminal` or `json:<path>` supported by the hosted runner.
    #[serde(default)]
    reports: Vec<String>,
    /// Maximum concurrent case executions; must be at least 1.
    #[schemars(range(min = 1))]
    parallel: u32,
    /// Apply CI exit-code semantics to the completed run.
    #[serde(default)]
    ci: bool,
    /// Evaluator names to run; omit to use the suite defaults.
    #[serde(default)]
    evaluators: Vec<String>,
    /// Optional hard cost ceiling in US dollars for each case execution.
    #[schemars(range(min = 0.0))]
    max_cost_dollars: Option<f64>,
    /// Optional hard latency ceiling in milliseconds for each case execution.
    #[schemars(range(min = 1))]
    max_latency_ms: Option<u64>,
    /// Optional hard total-token ceiling for each case execution.
    #[schemars(range(min = 1))]
    max_tokens: Option<u64>,
    /// Optional hard tool-call ceiling for each case execution.
    #[schemars(range(min = 0))]
    max_tool_calls: Option<u64>,
    /// Optional hard conversational-turn ceiling for each case execution.
    #[schemars(range(min = 1))]
    max_turns: Option<u64>,
    /// Include per-case responses and score comments in terminal results.
    #[serde(default)]
    verbose: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct EvalRunIdInput {
    /// Hosted eval run UUID returned by `eval_run`.
    run_id: Uuid,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct EvalCompareInput {
    /// Completed hosted eval run UUID used as the baseline.
    base_run: Uuid,
    /// Completed hosted eval run UUID containing the candidate behavior.
    new_run: Uuid,
}

#[tool_router(router = evals_router)]
impl Server {
    /// Parse and summarize caller-supplied inline TOML eval suite documents.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn eval_suites_summarize(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<EvalSuitesInput>,
    ) -> CallToolResult {
        let input = serde_json::json!({
            "documents": input.documents.into_iter().map(EvalSuiteListDocument::from).collect::<Vec<_>>()
        });
        self.tenant_command::<_, EvalSuiteListRequest, EvalSuiteListResponse>(
            context,
            &input,
            EVAL_SUITES_LIST,
            "Summarized eval suites.",
        )
        .await
    }

    /// Plan an inline TOML eval suite and estimate run count and cost without executing it.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn eval_plan(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<EvalPlanInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, EvalPlanRequest, EvalPlanResponse>(
            context,
            &input,
            EVAL_PLAN,
            "Planned eval suite.",
        )
        .await
    }

    /// List persisted eval datasets for the authenticated tenant.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn eval_datasets_list(&self, context: RequestContext<RoleServer>) -> CallToolResult {
        self.tenant_command::<_, EvalDatasetListRequest, EvalDatasetListResponse>(
            context,
            &EmptyInput {},
            EVAL_DATASETS_LIST,
            "Listed eval datasets.",
        )
        .await
    }

    /// Register a tenant-scoped JSONL eval dataset.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = false
    ))]
    async fn eval_dataset_register(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<EvalDatasetRegisterInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, EvalDatasetRegisterRequest, EvalDatasetRegisterResponse>(
            context,
            &input,
            EVAL_DATASET_REGISTER,
            "Registered eval dataset.",
        )
        .await
    }

    /// Accept an internal hosted eval run; poll `eval_run_status` using the returned ID.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = false,
        open_world_hint = true
    ))]
    async fn eval_run(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<EvalRunInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, EvalRunRequest, EvalRunResponse>(
            context,
            &input,
            EVAL_RUN,
            "Accepted eval run.",
        )
        .await
    }

    /// Poll one accepted hosted eval run.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn eval_run_status(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<EvalRunIdInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, EvalRunStatusRequest, EvalRunStatusResponse>(
            context,
            &input,
            EVAL_RUN_STATUS,
            "Loaded eval run status.",
        )
        .await
    }

    /// Read score summaries for a completed eval run.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn eval_scores(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<EvalRunIdInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, EvalScoresRequest, EvalScoresResponse>(
            context,
            &input,
            EVAL_SCORES,
            "Loaded eval scores.",
        )
        .await
    }

    /// Compare score summaries for two completed eval runs.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn eval_compare(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<EvalCompareInput>,
    ) -> CallToolResult {
        self.tenant_command::<_, EvalCompareRequest, EvalCompareResponse>(
            context,
            &input,
            EVAL_COMPARE,
            "Compared eval runs.",
        )
        .await
    }
}
