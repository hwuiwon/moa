//! `xtask check-architecture-boundaries` command implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

const SCAN_ROOTS: &[&str] = &[
    "crates/moa-orchestrator/src/objects",
    "crates/moa-orchestrator/src/services",
    "crates/moa-orchestrator/src/workflows",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    DirectSql,
    EventWildcardMatch,
    ForbiddenDependency,
    HandlerAuthzSafety,
    LocBudget,
    RuntimeContext,
    ReverseDependencyBudget,
    SymbolBudget,
    WorkspaceBudget,
}

impl fmt::Display for Rule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectSql => formatter.write_str("direct SQL in handler/workflow code"),
            Self::EventWildcardMatch => {
                formatter.write_str("wildcard Event match in sensitive consumer")
            }
            Self::ForbiddenDependency => formatter.write_str("forbidden dependency direction"),
            Self::HandlerAuthzSafety => {
                formatter.write_str("Restate handler without authz or SAFETY marker")
            }
            Self::LocBudget => formatter.write_str("LOC budget"),
            Self::RuntimeContext => formatter.write_str("raw OrchestratorCtx dependency access"),
            Self::ReverseDependencyBudget => formatter.write_str("reverse dependency budget"),
            Self::SymbolBudget => formatter.write_str("symbol budget"),
            Self::WorkspaceBudget => formatter.write_str("workspace package budget"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Allowance {
    rule: Rule,
    path: &'static str,
    needle: &'static str,
    expected_count: usize,
    reason: &'static str,
}

macro_rules! allow {
    ($rule:ident, $path:literal, $needle:literal, $expected_count:literal, $reason:literal) => {
        Allowance {
            rule: Rule::$rule,
            path: $path,
            needle: $needle,
            expected_count: $expected_count,
            reason: $reason,
        }
    };
}

// Each entry is a counted exception. Increasing direct SQL or raw dependency
// access under an allowed file still fails until a reviewer records a reason.
const ALLOWANCES: &[Allowance] = &[
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/objects/sub_agent/request.rs",
        "OrchestratorCtx::current_tool_schemas",
        1,
        "Sub-agent request prep still reads configured tool schemas from the runtime singleton"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/objects/sub_agent/request.rs",
        "OrchestratorCtx::current_provider_registry",
        1,
        "Sub-agent model capability checks still use the shared provider registry"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/objects/session/handlers.rs",
        "OrchestratorCtx::current_session_store",
        1,
        "Session handlers still use the session-store seam for direct status and progress reads"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/objects/tenant.rs",
        "OrchestratorCtx::current()",
        2,
        "Tenant VO still owns a narrow memory-summary read and action-policy persistence pending repository seams"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/objects/tenant.rs",
        ".graph_pool()",
        1,
        "Tenant VO memory-summary read currently obtains the graph pool from grouped deps"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/objects/tenant.rs",
        "sqlx::query_scalar",
        1,
        "Tenant VO memory summary has one direct graph-node count pending repository extraction"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "OrchestratorCtx::current_graph_pool",
        6,
        "Restate adapter obtains the pool for the extracted action-review app/store seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/action_reviews.rs",
        "OrchestratorCtx::current_session_store",
        2,
        "Action-review terminal event append still uses the Restate session-store client seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/admin_maintenance.rs",
        "OrchestratorCtx::current()",
        1,
        "Maintenance handler needs grouped runtime deps until the maintenance app seam exists"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/admin_maintenance.rs",
        ".graph_pool()",
        1,
        "Maintenance handler reads the graph pool from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/admin_maintenance.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Remaining maintenance actions still call repository constructors directly"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/admin_maintenance.rs",
        "OrchestratorCtx::current_config",
        4,
        "Maintenance jobs still read database config from runtime config accessors"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/agents.rs",
        "OrchestratorCtx::current_graph_pool",
        6,
        "Agent service is a thin authz/DTO adapter over identity_admin repositories"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/agent_definitions.rs",
        "OrchestratorCtx::current_graph_pool",
        5,
        "Agent-definition service currently owns artifact-backed install/deploy repository operations"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/agent_definitions.rs",
        "sqlx::query(",
        7,
        "Agent installation and deployment SQL remains local pending an agent-definition repository seam"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/agent_definitions.rs",
        "sqlx::query_scalar",
        2,
        "Agent installation guard SQL remains local pending an agent-definition repository seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/analytics/mod.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Analytics experiment endpoint still constructs the experiment analytics read model"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/analytics/mod.rs",
        "OrchestratorCtx::current_session_store",
        6,
        "Analytics service reads session, cache, tool, learning, and search projections through the current store seam"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/analytics/experiment_stats.rs",
        "sqlx::query(",
        4,
        "Experiment analytics read-model SQL remains local pending a dedicated analytics repository"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/api_keys.rs",
        "OrchestratorCtx::current()",
        2,
        "API-key service needs FGA plus repository deps for operator-scope checks"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/api_keys.rs",
        "OrchestratorCtx::current_graph_pool",
        4,
        "API-key persistence moved under identity_admin while the handler still constructs repos"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/artifacts.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Artifact registry is the current domain API and requires the Postgres pool"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/audit.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "OCSF security audit verification has a narrow local repository exception"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/audit.rs",
        "sqlx::query_as",
        2,
        "Audit verification only does tenant lookup before authz and payload lookup after authz"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/authz_admin.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Authz admin handler still constructs authz repository state directly"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/authz_challenges.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Authz challenge adapter obtains the pool for the extracted app/store seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/contacts.rs",
        "OrchestratorCtx::current_graph_pool",
        10,
        "Contact service constructs the initial in-process contact repository operations"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/contacts.rs",
        "OrchestratorCtx::current_session_store",
        7,
        "Contact service validates and updates session contact bindings through the session-store seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/contacts.rs",
        "OrchestratorCtx::current_config",
        2,
        "Contact verification TTL and contact-point hash key env name are read from runtime config"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/contacts.rs",
        "OrchestratorCtx::current()",
        1,
        "Contact token issuer is read from the process provider bundle"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/contacts.rs",
        ".auth_providers()",
        1,
        "Contact token issuer is stored on the auth provider bundle"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/eval/mod.rs",
        "OrchestratorCtx::current()",
        1,
        "Eval service still combines provider registry and analytics persistence"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/eval/mod.rs",
        ".graph_pool()",
        1,
        "Eval service reads the pool from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/eval/mod.rs",
        "OrchestratorCtx::current_config",
        2,
        "Internal eval runner still reads model and database config from runtime config accessors"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/eval/repository.rs",
        "QueryBuilder::<",
        1,
        "Eval repository owns dataset multi-row insert persistence"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/eval/mod.rs",
        "OrchestratorCtx::current_graph_pool",
        4,
        "Eval handler obtains pools before delegating to repositories and scoring read models"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/eval/repository.rs",
        "sqlx::query(",
        3,
        "Eval repository owns dataset SQL row mapping"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/eval/repository.rs",
        "sqlx::query_scalar",
        1,
        "Eval repository owns dataset upsert SQL"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/experiments.rs",
        "OrchestratorCtx::current()",
        2,
        "Experiment service needs grouped deps for behavior-lab app and workflow dispatch"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/experiments.rs",
        ".graph_pool()",
        2,
        "Experiment service reads the pool from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/experiments.rs",
        ".provider_registry()",
        1,
        "Experiment proposal generation still creates the local LLM gateway facade"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/experiments.rs",
        "OrchestratorCtx::current_graph_pool",
        10,
        "Experiment service still constructs extracted experiment app/store helpers per handler"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/graph_memory_maint.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Graph-memory maintenance is a storage-maintenance exception"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/graph_memory_maint.rs",
        "sqlx::query_scalar",
        1,
        "Graph-memory maintenance scans storage partitions until a maintenance repository owns it"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/learning_review.rs",
        "OrchestratorCtx::current()",
        3,
        "Learning review handler needs grouped deps and a concrete backend for transaction-aware skill promotion"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/learning_review.rs",
        ".provider_registry()",
        1,
        "Learning review regression gate still consumes the configured provider registry"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/learning_review.rs",
        ".graph_pool()",
        1,
        "Learning review acceptance passes the runtime pool into the extracted skill promotion flow"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/lineage_admin.rs",
        "OrchestratorCtx::current_graph_pool",
        5,
        "Lineage admin adapter keeps explicit authz/read-only transaction setup"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/lineage_admin.rs",
        "OrchestratorCtx::current_config",
        3,
        "Lineage export, verify, and erase handlers still read compliance config before delegating"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/lineage_admin.rs",
        "sqlx::query(",
        2,
        "Lineage admin only sets read-only transaction controls before lineage crate helpers run"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/knowledge/mod.rs",
        "OrchestratorCtx::current_config",
        2,
        "Knowledge production service constructors still read parser/provider config from runtime"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/knowledge/mod.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Knowledge production service constructors still obtain the Postgres pool from runtime"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/knowledge/inspect.rs",
        "sqlx::query(",
        1,
        "Knowledge query-trace inspection reads lineage diagnostics until an analytics repository owns it"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory/retrieval.rs",
        "OrchestratorCtx::current()",
        1,
        "Memory handler needs grouped pool access until the memory app seam is extracted"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory/retrieval.rs",
        ".graph_pool()",
        1,
        "Memory handler reads the graph pool from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory/retrieval.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Memory handler still constructs graph stores directly for the old memory endpoint"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory/retrieval.rs",
        "OrchestratorCtx::current_config",
        2,
        "Memory debug retrieval still reads lineage and embedder config from the runtime singleton"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory/retrieval.rs",
        "OrchestratorCtx::current_lineage",
        1,
        "Memory debug lineage endpoint directly records one lineage diagnostic event"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/neon_maint.rs",
        "OrchestratorCtx::current()",
        1,
        "Neon maintenance endpoint still needs grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/privacy/mod.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Privacy adapter keeps token/vault/export orchestration while erasure moved to owning crates"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/privacy/mod.rs",
        "OrchestratorCtx::current_config",
        2,
        "Privacy export and erase handlers still read compliance approval-token config"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/privacy/repository.rs",
        "sqlx::query(",
        2,
        "Privacy repository sets auditor role and resolves contact subjects before controlled export reads"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/privacy/repository.rs",
        "sqlx::query_scalar",
        7,
        "Privacy repository owns DSAR export read-model and linked-contact SQL"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/skills.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Skills service uses the current skill registry constructor"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/tenants.rs",
        "OrchestratorCtx::current_graph_pool",
        3,
        "Tenant service is a thin authz/DTO adapter over identity_admin repositories"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/tool_executor.rs",
        "OrchestratorCtx::current_session_store",
        3,
        "Tool executor still appends action events through the Restate session-store seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/workflows.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Workflow service constructs the artifact registry for workflow state"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/consolidate.rs",
        "OrchestratorCtx::current()",
        2,
        "Consolidation workflow needs grouped pool/embedder deps until memory app seam expands"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/consolidate.rs",
        ".graph_pool()",
        2,
        "Consolidation workflow reads the pool from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/consolidate.rs",
        "OrchestratorCtx::current_graph_pool",
        3,
        "Consolidation workflow still constructs graph-memory helpers directly"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/consolidate.rs",
        "OrchestratorCtx::current_session_store",
        1,
        "Consolidation workflow still reads recent events through the session-store seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/consolidate.rs",
        ".embedding_provider()",
        1,
        "Consolidation backfill still reads the configured embedder from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/artifact_workflow_execution.rs",
        "OrchestratorCtx::current_graph_pool",
        8,
        "Artifact workflow execution still constructs the artifact registry in workflow helper steps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_run.rs",
        "OrchestratorCtx::current_graph_pool",
        3,
        "Experiment workflow uses experiment app/store helpers from the global context"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_run/plan_expansion.rs",
        "OrchestratorCtx::current_graph_pool",
        5,
        "Experiment plan expansion still uses the experiment store from workflow steps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_run/status.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Experiment status projection still reads through experiment store helpers"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_run/status.rs",
        "OrchestratorCtx::current_session_store",
        1,
        "Experiment status projection still resolves source sessions"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Experiment target execution reads workflow/runtime stores and passes the pool into session creation helpers"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs",
        "OrchestratorCtx::current_session_store",
        1,
        "Experiment target execution still appends trial session events"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_trial_run.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Experiment trial workflow still reads experiment trial state through store helpers"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/status.rs",
        "OrchestratorCtx::current_graph_pool",
        6,
        "Experiment trial status projection still uses experiment store helpers"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Experiment trial target execution reads workflow/runtime stores and passes the pool into session creation helpers"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs",
        "OrchestratorCtx::current_session_store",
        3,
        "Experiment trial target execution still appends session events"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/trial_simulator.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Trial simulator still reads experiment state through store helpers"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/experiment_trial_run/trial_simulator.rs",
        "OrchestratorCtx::current_provider_registry",
        1,
        "Trial simulator still creates the local LLM gateway facade"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/skill_learning.rs",
        "OrchestratorCtx::current()",
        1,
        "Skill-learning workflow needs grouped store/provider deps for proposal generation"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/skill_learning.rs",
        ".provider_registry()",
        1,
        "Skill-learning workflow still uses configured providers for distillation"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/knowledge_sync_ingestion.rs",
        "OrchestratorCtx::current_graph_pool",
        4,
        "Knowledge sync ingestion workflow constructs repositories and ingestion runners inside durable steps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/knowledge_sync_ingestion.rs",
        "OrchestratorCtx::current_config",
        3,
        "Knowledge sync ingestion workflow reads provider/parser config inside durable steps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/progress_delivery.rs",
        "OrchestratorCtx::current_channel_adapter",
        1,
        "Progress delivery resolves the target channel adapter through the runtime registry"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/progress_delivery.rs",
        "OrchestratorCtx::current_session_store",
        1,
        "Progress delivery reads active channel bindings through the session-store seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/sub_agent_turn_execution.rs",
        "OrchestratorCtx::current_config",
        1,
        "Sub-agent turn execution still reads generation config from runtime config"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        "OrchestratorCtx::current()",
        3,
        "TurnExecution is the central workflow adapter and still needs grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        ".session_store()",
        1,
        "TurnExecution reads the session store from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        "OrchestratorCtx::current_session_store",
        4,
        "TurnExecution still appends workflow events through the current session-store seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        "OrchestratorCtx::current_tool_router",
        1,
        "TurnExecution still invokes the in-process tool router"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        "OrchestratorCtx::current_tool_schemas",
        1,
        "TurnExecution reports available tool count from the runtime tool schema registry"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        "OrchestratorCtx::current_lineage",
        1,
        "TurnExecution still obtains the lineage handle while generation lineage is emitted inline"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        "OrchestratorCtx::current_config",
        6,
        "TurnExecution still reads generation and resolution config from the runtime singleton"
    ),
];

const WORKSPACE_PACKAGE_COUNT_BUDGET: usize = 43;
const WORKSPACE_DEFAULT_MEMBER_COUNT_BUDGET: usize = 40;

const REVERSE_DEPENDENCY_BUDGETS: &[ReverseDependencyBudget] = &[ReverseDependencyBudget {
    package: "moa-core",
    max_direct: 36,
    max_transitive: 38,
    reason: "moa-core is the shared trait/DTO crate; new fan-in should be intentional",
}];

const LOC_BUDGETS: &[LocBudget] = &[
    LocBudget {
        label: "moa-core Rust source",
        path: "crates/moa-core/src",
        scope: LocScope::RustTree,
        max_lines: 18_170,
        reason: "moa-core has high workspace fan-in; current budget includes RLS context, env-overlay delegation, and narrow session repository traits without new re-exports",
    },
    LocBudget {
        label: "public edge route ladder",
        path: "crates/moa-edge/src/routes.rs",
        scope: LocScope::File,
        max_lines: 3_886,
        reason: "routes.rs is a known merge-conflict hotspot pending route decomposition",
    },
    LocBudget {
        label: "moa-core env overlay",
        path: "crates/moa-core/src/config/env_overlay.rs",
        scope: LocScope::File,
        max_lines: 1_057,
        reason: "env_overlay.rs owns only the flat DTO, parsing, composition, and regression tests after per-domain delegation",
    },
    LocBudget {
        label: "turn execution workflow",
        path: "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        scope: LocScope::File,
        max_lines: 2_065,
        reason: "TurnExecution is the central durable workflow pending collaborator extraction",
    },
];

const SYMBOL_BUDGETS: &[SymbolBudget] = &[SymbolBudget {
    label: "moa-core top-level pub use exports",
    path: "crates/moa-core/src/lib.rs",
    max_count: 77,
    reason: "top-level re-export growth widens the moa-core compatibility wall",
}];

const NON_DOMAIN_ORCHESTRATOR_DEPENDENTS: &[&str] = &[
    "moa-orchestrator",
    "moa-edge",
    "moa-loadtest",
    "moa-test-support",
    "moa-fga-bootstrap",
    "xtask",
    "workspace-hack",
];

const FORBIDDEN_DEPENDENCY_RULES: &[ForbiddenDependencyRule] = &[
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-core"),
        target: DependencySelector::Prefix("moa-memory-"),
        reason: "docs/15 keeps memory-owned graph/vector/PII/ingest types out of moa-core",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::WorkspaceExcept(NON_DOMAIN_ORCHESTRATOR_DEPENDENTS),
        target: DependencySelector::Exact("moa-orchestrator"),
        reason: "docs/15 keeps moa-orchestrator as the Restate transport/workflow/composition boundary",
    },
];

const SENSITIVE_EVENT_CONSUMERS: &[SensitiveEventConsumer] = &[
    SensitiveEventConsumer {
        path: "crates/moa-orchestrator/src/services/analytics/redaction.rs",
        max_wildcard_event_match_arms: 0,
        reason: "analytics redaction must make every Event variant's preview behavior explicit",
    },
    SensitiveEventConsumer {
        path: "crates/moa-session/src/store/session_store.rs",
        max_wildcard_event_match_arms: 0,
        reason: "session persistence must not silently ignore new Event variants in sensitive handling",
    },
    SensitiveEventConsumer {
        path: "crates/moa-hands/src/tools/session_search.rs",
        max_wildcard_event_match_arms: 2,
        reason: "existing session_search wildcard Event arms are baseline debt; new wildcard arms are rejected until the hand-tool owner makes it exhaustive",
    },
];

#[derive(Debug, Clone, Copy)]
struct ReverseDependencyBudget {
    package: &'static str,
    max_direct: usize,
    max_transitive: usize,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct LocBudget {
    label: &'static str,
    path: &'static str,
    scope: LocScope,
    max_lines: usize,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
enum LocScope {
    File,
    RustTree,
}

#[derive(Debug, Clone, Copy)]
struct SymbolBudget {
    label: &'static str,
    path: &'static str,
    max_count: usize,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct ForbiddenDependencyRule {
    source: DependencySelector,
    target: DependencySelector,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct SensitiveEventConsumer {
    path: &'static str,
    max_wildcard_event_match_arms: usize,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
enum DependencySelector {
    Exact(&'static str),
    Prefix(&'static str),
    WorkspaceExcept(&'static [&'static str]),
}

impl DependencySelector {
    fn matches(self, package: &str, graph: &PackageGraph) -> bool {
        match self {
            Self::Exact(expected) => package == expected,
            Self::Prefix(prefix) => package.starts_with(prefix),
            Self::WorkspaceExcept(excluded) => {
                graph.workspace_members.contains(package) && !excluded.contains(&package)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct PackageGraph {
    workspace_members: BTreeSet<String>,
    default_members: BTreeSet<String>,
    dependencies: BTreeMap<String, BTreeSet<String>>,
}

impl PackageGraph {
    fn package_count(&self) -> usize {
        self.workspace_members.len()
    }

    fn default_member_count(&self) -> usize {
        self.default_members.len()
    }

    fn direct_reverse_dependencies(&self, package: &str) -> BTreeSet<String> {
        self.dependencies
            .iter()
            .filter(|(candidate, dependencies)| {
                candidate.as_str() != package && dependencies.contains(package)
            })
            .map(|(candidate, _dependencies)| candidate.clone())
            .collect()
    }

    fn transitive_reverse_dependencies(&self, package: &str) -> BTreeSet<String> {
        self.workspace_members
            .iter()
            .filter(|candidate| candidate.as_str() != package)
            .filter(|candidate| self.depends_on(candidate, package))
            .cloned()
            .collect()
    }

    fn depends_on(&self, source: &str, target: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = self
            .dependencies
            .get(source)
            .into_iter()
            .flat_map(|dependencies| dependencies.iter().cloned())
            .collect::<Vec<_>>();

        while let Some(candidate) = stack.pop() {
            if candidate == target {
                return true;
            }
            if !seen.insert(candidate.clone()) {
                continue;
            }
            if let Some(dependencies) = self.dependencies.get(&candidate) {
                stack.extend(dependencies.iter().cloned());
            }
        }

        false
    }

    #[cfg(test)]
    fn for_tests(packages: &[&str], default_members: &[&str], edges: &[(&str, &str)]) -> Self {
        let workspace_members = packages
            .iter()
            .map(|package| (*package).to_string())
            .collect::<BTreeSet<_>>();
        let default_members = default_members
            .iter()
            .map(|package| (*package).to_string())
            .collect::<BTreeSet<_>>();
        let mut dependencies = workspace_members
            .iter()
            .map(|package| (package.clone(), BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();

        for (source, target) in edges {
            dependencies
                .entry((*source).to_string())
                .or_default()
                .insert((*target).to_string());
        }

        Self {
            workspace_members,
            default_members,
            dependencies,
        }
    }
}

#[derive(Debug)]
struct ArchitectureReport {
    workspace_package_count: usize,
    default_member_count: usize,
    reverse_dependencies: Vec<ReverseDependencyReport>,
    loc_budgets: Vec<LocBudgetReport>,
    symbol_budgets: Vec<SymbolBudgetReport>,
    forbidden_dependency_rule_count: usize,
}

impl ArchitectureReport {
    fn display(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "workspace packages: {} / {} budget; default members: {} / {} budget",
            self.workspace_package_count,
            WORKSPACE_PACKAGE_COUNT_BUDGET,
            self.default_member_count,
            WORKSPACE_DEFAULT_MEMBER_COUNT_BUDGET
        ));
        for report in &self.reverse_dependencies {
            lines.push(format!(
                "{} reverse dependencies: {} direct / {} budget, {} transitive / {} budget; reason: {}",
                report.package,
                report.direct_count,
                report.max_direct,
                report.transitive_count,
                report.max_transitive,
                report.reason
            ));
        }
        for report in &self.loc_budgets {
            lines.push(format!(
                "{} LOC: {} / {} budget at {}; reason: {}",
                report.label, report.lines, report.max_lines, report.path, report.reason
            ));
        }
        for report in &self.symbol_budgets {
            lines.push(format!(
                "{}: {} / {} budget at {}; reason: {}",
                report.label, report.count, report.max_count, report.path, report.reason
            ));
        }
        lines.push(format!(
            "forbidden dependency direction rules checked: {}",
            self.forbidden_dependency_rule_count
        ));
        lines.join("\n")
    }
}

#[derive(Debug)]
struct ReverseDependencyReport {
    package: &'static str,
    direct_count: usize,
    transitive_count: usize,
    max_direct: usize,
    max_transitive: usize,
    reason: &'static str,
}

#[derive(Debug)]
struct LocBudgetReport {
    label: &'static str,
    path: &'static str,
    lines: usize,
    max_lines: usize,
    reason: &'static str,
}

#[derive(Debug)]
struct SymbolBudgetReport {
    label: &'static str,
    path: &'static str,
    count: usize,
    max_count: usize,
    reason: &'static str,
}

/// Runs the architecture-boundary scanner.
pub fn run() -> Result<()> {
    let mut findings = scan_roots(SCAN_ROOTS)?;
    findings.extend(scan_sensitive_event_consumers(
        Path::new("."),
        SENSITIVE_EVENT_CONSUMERS,
    )?);
    let (report, budget_findings) = scan_architecture_budgets()?;
    findings.extend(budget_findings);
    if findings.is_empty() {
        println!(
            "architecture boundary checks clean:\n{}\nallowlisted orchestrator exception groups checked: {}",
            report.display(),
            ALLOWANCES.len()
        );
        return Ok(());
    }

    bail!(
        "architecture boundary violations detected:\n{}\n\ncurrent architecture budget report:\n{}",
        findings
            .iter()
            .map(Finding::display)
            .collect::<Vec<_>>()
            .join("\n"),
        report.display()
    )
}

fn scan_architecture_budgets() -> Result<(ArchitectureReport, Vec<Finding>)> {
    let graph = load_package_graph()?;
    scan_architecture_budgets_with_graph(&graph, Path::new("."))
}

fn scan_architecture_budgets_with_graph(
    graph: &PackageGraph,
    root: &Path,
) -> Result<(ArchitectureReport, Vec<Finding>)> {
    let mut findings = Vec::new();
    if graph.package_count() > WORKSPACE_PACKAGE_COUNT_BUDGET {
        findings.push(Finding::budget(
            Rule::WorkspaceBudget,
            "Cargo metadata",
            format!(
                "workspace package count exceeded budget: expected at most {}, saw {}",
                WORKSPACE_PACKAGE_COUNT_BUDGET,
                graph.package_count()
            ),
        ));
    }
    if graph.default_member_count() > WORKSPACE_DEFAULT_MEMBER_COUNT_BUDGET {
        findings.push(Finding::budget(
            Rule::WorkspaceBudget,
            "Cargo metadata",
            format!(
                "workspace default-member count exceeded budget: expected at most {}, saw {}",
                WORKSPACE_DEFAULT_MEMBER_COUNT_BUDGET,
                graph.default_member_count()
            ),
        ));
    }

    let (reverse_dependencies, reverse_findings) =
        reverse_dependency_budget_reports(graph, REVERSE_DEPENDENCY_BUDGETS);
    findings.extend(reverse_findings);
    let (loc_budgets, loc_findings) = loc_budget_reports(root, LOC_BUDGETS)?;
    findings.extend(loc_findings);
    let (symbol_budgets, symbol_findings) = symbol_budget_reports(root, SYMBOL_BUDGETS)?;
    findings.extend(symbol_findings);
    findings.extend(forbidden_dependency_findings(
        graph,
        FORBIDDEN_DEPENDENCY_RULES,
    ));

    Ok((
        ArchitectureReport {
            workspace_package_count: graph.package_count(),
            default_member_count: graph.default_member_count(),
            reverse_dependencies,
            loc_budgets,
            symbol_budgets,
            forbidden_dependency_rule_count: FORBIDDEN_DEPENDENCY_RULES.len(),
        },
        findings,
    ))
}

fn load_package_graph() -> Result<PackageGraph> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .output()
        .context("run cargo metadata for architecture boundary check")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_package_graph(&output.stdout)
}

fn parse_package_graph(metadata_json: &[u8]) -> Result<PackageGraph> {
    let metadata = serde_json::from_slice::<Value>(metadata_json)
        .context("parse cargo metadata JSON for architecture boundary check")?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .context("cargo metadata JSON missing `packages` array")?;

    let mut id_to_name = BTreeMap::new();
    let mut package_values = BTreeMap::new();
    for package in packages {
        let name = value_string_field(package, "name")?.to_string();
        let id = value_string_field(package, "id")?.to_string();
        id_to_name.insert(id, name.clone());
        package_values.insert(name, package);
    }

    let workspace_members =
        package_names_from_metadata_ids(&metadata, "workspace_members", &id_to_name)?;
    let default_members =
        package_names_from_metadata_ids(&metadata, "workspace_default_members", &id_to_name)?;
    let mut dependencies = BTreeMap::new();
    for package_name in &workspace_members {
        let Some(package) = package_values.get(package_name) else {
            bail!("workspace package `{package_name}` missing from cargo metadata package list");
        };
        let package_dependencies = package
            .get("dependencies")
            .and_then(Value::as_array)
            .with_context(|| format!("package `{package_name}` missing dependencies array"))?;
        let workspace_dependencies = package_dependencies
            .iter()
            .map(|dependency| value_string_field(dependency, "name"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|dependency_name| workspace_members.contains(*dependency_name))
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        dependencies.insert(package_name.clone(), workspace_dependencies);
    }

    Ok(PackageGraph {
        workspace_members,
        default_members,
        dependencies,
    })
}

fn package_names_from_metadata_ids(
    metadata: &Value,
    field: &str,
    id_to_name: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
    let ids = metadata
        .get(field)
        .and_then(Value::as_array)
        .with_context(|| format!("cargo metadata JSON missing `{field}` array"))?;
    ids.iter()
        .map(|value| {
            let id = value
                .as_str()
                .with_context(|| format!("cargo metadata `{field}` contains a non-string id"))?;
            id_to_name
                .get(id)
                .cloned()
                .with_context(|| format!("cargo metadata `{field}` references unknown id `{id}`"))
        })
        .collect()
}

fn value_string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("cargo metadata value missing string field `{field}`"))
}

fn reverse_dependency_budget_reports(
    graph: &PackageGraph,
    budgets: &[ReverseDependencyBudget],
) -> (Vec<ReverseDependencyReport>, Vec<Finding>) {
    let mut reports = Vec::new();
    let mut findings = Vec::new();
    for budget in budgets {
        let direct_count = graph.direct_reverse_dependencies(budget.package).len();
        let transitive_count = graph.transitive_reverse_dependencies(budget.package).len();
        reports.push(ReverseDependencyReport {
            package: budget.package,
            direct_count,
            transitive_count,
            max_direct: budget.max_direct,
            max_transitive: budget.max_transitive,
            reason: budget.reason,
        });
        if direct_count > budget.max_direct {
            findings.push(Finding::budget(
                Rule::ReverseDependencyBudget,
                "Cargo metadata",
                format!(
                    "{} direct reverse dependency budget exceeded: expected at most {}, saw {}; reason: {}",
                    budget.package, budget.max_direct, direct_count, budget.reason
                ),
            ));
        }
        if transitive_count > budget.max_transitive {
            findings.push(Finding::budget(
                Rule::ReverseDependencyBudget,
                "Cargo metadata",
                format!(
                    "{} transitive reverse dependency budget exceeded: expected at most {}, saw {}; reason: {}",
                    budget.package, budget.max_transitive, transitive_count, budget.reason
                ),
            ));
        }
    }

    (reports, findings)
}

fn loc_budget_reports(
    root: &Path,
    budgets: &[LocBudget],
) -> Result<(Vec<LocBudgetReport>, Vec<Finding>)> {
    let mut reports = Vec::new();
    let mut findings = Vec::new();
    for budget in budgets {
        let path = root.join(budget.path);
        let lines = count_loc(&path, budget.scope)
            .with_context(|| format!("count LOC budget `{}` at {}", budget.label, budget.path))?;
        reports.push(LocBudgetReport {
            label: budget.label,
            path: budget.path,
            lines,
            max_lines: budget.max_lines,
            reason: budget.reason,
        });
        if lines > budget.max_lines {
            findings.push(Finding::budget(
                Rule::LocBudget,
                budget.path,
                format!(
                    "{} LOC budget exceeded: expected at most {}, saw {}; reason: {}",
                    budget.label, budget.max_lines, lines, budget.reason
                ),
            ));
        }
    }

    Ok((reports, findings))
}

fn count_loc(path: &Path, scope: LocScope) -> Result<usize> {
    match scope {
        LocScope::File => count_file_lines(path),
        LocScope::RustTree => {
            if !path.exists() {
                bail!("LOC budget path does not exist: {}", path.display());
            }
            let mut files = Vec::new();
            collect_rust_files(path, &mut files)?;
            files
                .iter()
                .map(|file| count_file_lines(file))
                .try_fold(0usize, |total, count| count.map(|count| total + count))
        }
    }
}

fn count_file_lines(path: &Path) -> Result<usize> {
    let source = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(source.lines().count())
}

fn symbol_budget_reports(
    root: &Path,
    budgets: &[SymbolBudget],
) -> Result<(Vec<SymbolBudgetReport>, Vec<Finding>)> {
    let mut reports = Vec::new();
    let mut findings = Vec::new();
    for budget in budgets {
        let path = root.join(budget.path);
        let source =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let count = count_pub_use_exports(&source);
        reports.push(SymbolBudgetReport {
            label: budget.label,
            path: budget.path,
            count,
            max_count: budget.max_count,
            reason: budget.reason,
        });
        if let Some(finding) = symbol_budget_finding(*budget, count) {
            findings.push(finding);
        }
    }

    Ok((reports, findings))
}

fn symbol_budget_finding(budget: SymbolBudget, count: usize) -> Option<Finding> {
    (count > budget.max_count).then(|| {
        Finding::budget(
            Rule::SymbolBudget,
            budget.path,
            format!(
                "{} symbol budget exceeded: expected at most {}, saw {}; reason: {}",
                budget.label, budget.max_count, count, budget.reason
            ),
        )
    })
}

fn count_pub_use_exports(source: &str) -> usize {
    let mut count = 0;
    let mut statement = String::new();
    let mut in_pub_use = false;

    for line in source.lines() {
        let trimmed = line.trim();
        if !in_pub_use && trimmed.starts_with("pub use ") {
            statement.clear();
            statement.push_str(trimmed);
            in_pub_use = true;
        } else if in_pub_use {
            statement.push(' ');
            statement.push_str(trimmed);
        }

        if in_pub_use && trimmed.ends_with(';') {
            count += count_pub_use_statement_exports(&statement);
            statement.clear();
            in_pub_use = false;
        }
    }

    count
}

fn count_pub_use_statement_exports(statement: &str) -> usize {
    let Some(exports) = statement
        .trim()
        .strip_prefix("pub use ")
        .map(str::trim)
        .map(|value| value.trim_end_matches(';').trim())
    else {
        return 0;
    };

    let Some(open_brace) = exports.find('{') else {
        return 1;
    };
    let Some(close_brace) = exports.rfind('}') else {
        return 1;
    };
    count_top_level_comma_items(&exports[open_brace + 1..close_brace])
}

fn count_top_level_comma_items(source: &str) -> usize {
    let mut count = 0;
    let mut depth = 0usize;
    let mut has_item = false;

    for character in source.chars() {
        match character {
            '{' => {
                depth += 1;
                has_item = true;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                has_item = true;
            }
            ',' if depth == 0 && has_item => {
                count += 1;
                has_item = false;
            }
            character if !character.is_whitespace() => {
                has_item = true;
            }
            _ => {}
        }
    }

    if has_item {
        count += 1;
    }

    count
}

fn forbidden_dependency_findings(
    graph: &PackageGraph,
    rules: &[ForbiddenDependencyRule],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (source, dependencies) in &graph.dependencies {
        for target in dependencies {
            for rule in rules {
                if rule.source.matches(source, graph) && rule.target.matches(target, graph) {
                    findings.push(Finding::budget(
                        Rule::ForbiddenDependency,
                        "Cargo metadata",
                        format!(
                            "forbidden workspace dependency `{source} -> {target}`; reason: {}",
                            rule.reason
                        ),
                    ));
                    break;
                }
            }
        }
    }
    findings
}

fn scan_sensitive_event_consumers(
    root: &Path,
    consumers: &[SensitiveEventConsumer],
) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for consumer in consumers {
        let path = root.join(consumer.path);
        let source =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let wildcard_arms = event_wildcard_match_arms(&source);
        if wildcard_arms.len() <= consumer.max_wildcard_event_match_arms {
            continue;
        }
        findings.push(Finding::budget(
            Rule::EventWildcardMatch,
            consumer.path,
            format!(
                "wildcard Event match arm budget exceeded: expected at most {}, saw {}; reason: {}; wildcard arms: {}",
                consumer.max_wildcard_event_match_arms,
                wildcard_arms.len(),
                consumer.reason,
                wildcard_arms
                    .iter()
                    .map(|arm| format!("line {} `{}`", arm.line, arm.source.trim()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    Ok(findings)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventWildcardMatchArm {
    line: usize,
    source: String,
}

fn event_wildcard_match_arms(source: &str) -> Vec<EventWildcardMatchArm> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut arms = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        if !line.contains("match ") || !line.contains("event") {
            index += 1;
            continue;
        }

        let (end_index, block) = collect_match_block(&lines, index);
        if block
            .iter()
            .any(|(_, block_line)| block_line.contains("Event::"))
        {
            arms.extend(
                block
                    .iter()
                    .filter(|(_, block_line)| is_wildcard_match_arm(block_line))
                    .map(|(line_index, block_line)| EventWildcardMatchArm {
                        line: line_index + 1,
                        source: (*block_line).to_string(),
                    }),
            );
        }
        index = end_index.max(index + 1);
    }
    arms
}

fn collect_match_block<'a>(lines: &'a [&'a str], start: usize) -> (usize, Vec<(usize, &'a str)>) {
    let mut block = Vec::new();
    let mut brace_depth = 0i32;
    let mut opened = false;
    let mut index = start;
    while index < lines.len() {
        let line = lines[index];
        block.push((index, line));
        brace_depth += brace_delta(line);
        opened |= line.contains('{');
        index += 1;
        if opened && brace_depth <= 0 {
            break;
        }
    }
    (index, block)
}

fn is_wildcard_match_arm(line: &str) -> bool {
    let trimmed = line.trim_start().trim_start_matches('|').trim_start();
    let Some((pattern, _)) = trimmed.split_once("=>") else {
        return false;
    };
    let pattern = pattern
        .trim()
        .split_once(" if ")
        .map_or(pattern.trim(), |(pattern, _)| pattern.trim());
    pattern == "_" || is_plain_identifier_pattern(pattern)
}

fn is_plain_identifier_pattern(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_lowercase())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn scan_roots(roots: &[&str]) -> Result<Vec<Finding>> {
    let mut files = Vec::new();
    for root in roots {
        collect_rust_files(Path::new(root), &mut files)?;
    }
    files.sort();

    let mut findings = Vec::new();
    let mut allowance_uses = vec![0usize; ALLOWANCES.len()];
    let service_traits = collect_restate_service_traits(&files)?;
    for path in files {
        scan_file(&path, &service_traits, &mut allowance_uses, &mut findings)?;
    }
    for (index, allowance) in ALLOWANCES.iter().enumerate() {
        let used = allowance_uses[index];
        if used != allowance.expected_count {
            findings.push(Finding::stale_allowance(*allowance, used));
        }
    }

    Ok(findings)
}

fn scan_file(
    path: &Path,
    service_traits: &BTreeSet<String>,
    allowance_uses: &mut [usize],
    findings: &mut Vec<Finding>,
) -> Result<()> {
    if path.file_name().and_then(|name| name.to_str()) == Some("tests.rs") {
        return Ok(());
    }

    let path_text = normalize_path(path);
    let body = fs::read_to_string(path).with_context(|| format!("read {path_text}"))?;
    scan_source(&path_text, &body, service_traits, allowance_uses, findings);
    Ok(())
}

fn scan_source(
    path_text: &str,
    body: &str,
    service_traits: &BTreeSet<String>,
    allowance_uses: &mut [usize],
    findings: &mut Vec<Finding>,
) {
    for (line_index, line) in body.lines().enumerate() {
        let Some(rule) = classify_line(line) else {
            continue;
        };
        let Some(allowance_index) = matching_allowance(rule, path_text, line) else {
            findings.push(Finding::new(
                rule,
                path_text.to_string(),
                line_index + 1,
                line,
            ));
            continue;
        };
        allowance_uses[allowance_index] += 1;
        if allowance_uses[allowance_index] > ALLOWANCES[allowance_index].expected_count {
            findings.push(Finding::exceeded_allowance(
                ALLOWANCES[allowance_index],
                allowance_uses[allowance_index],
                line_index + 1,
                line,
            ));
        }
    }
    if path_text.starts_with("crates/moa-orchestrator/src/services/") {
        findings.extend(handler_authz_safety_findings(
            path_text,
            body,
            service_traits,
        ));
    }
}

fn collect_restate_service_traits(files: &[PathBuf]) -> Result<BTreeSet<String>> {
    let mut service_traits = BTreeSet::new();
    for path in files {
        if path.file_name().and_then(|name| name.to_str()) == Some("tests.rs") {
            continue;
        }
        let path_text = normalize_path(path);
        let body = fs::read_to_string(path).with_context(|| format!("read {path_text}"))?;
        service_traits.extend(restate_service_traits_from_source(&body));
    }
    Ok(service_traits)
}

fn restate_service_traits_from_source(source: &str) -> BTreeSet<String> {
    let mut service_traits = BTreeSet::new();
    let mut pending_service_attr = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[restate_sdk::service") {
            pending_service_attr = true;
            continue;
        }
        if !pending_service_attr {
            continue;
        }
        if trimmed.starts_with("#[") || trimmed.starts_with("///") || trimmed.is_empty() {
            continue;
        }
        if let Some(trait_name) = service_trait_name(trimmed) {
            service_traits.insert(trait_name.to_string());
        }
        pending_service_attr = false;
    }
    service_traits
}

fn service_trait_name(line: &str) -> Option<&str> {
    let line = line
        .strip_prefix("pub trait ")
        .or_else(|| line.strip_prefix("trait "))?;
    line.split(|character: char| {
        character.is_whitespace() || matches!(character, '<' | '{' | ':' | '(')
    })
    .find(|part| !part.is_empty())
}

fn handler_authz_safety_findings(
    path_text: &str,
    source: &str,
    service_traits: &BTreeSet<String>,
) -> Vec<Finding> {
    if path_text.contains("/tests/") {
        return Vec::new();
    }

    let lines = source.lines().collect::<Vec<_>>();
    let scan_limit = lines
        .iter()
        .position(|line| line.trim_start().starts_with("#[cfg(test)]"))
        .unwrap_or(lines.len());
    let mut findings = Vec::new();
    let mut index = 0;

    while index < scan_limit {
        let line = lines[index].trim_start();
        if !is_service_impl(line, service_traits) {
            index += 1;
            continue;
        }

        let (impl_end, method_starts) = service_impl_methods(&lines, index, scan_limit);
        for (method_index, method_start) in method_starts.iter().copied().enumerate() {
            let method_end = method_starts
                .get(method_index + 1)
                .copied()
                .unwrap_or(impl_end);
            let body = lines[method_start..method_end].join("\n");
            if has_immediate_safety_comment(&lines, method_start) || has_authz_boundary(&body) {
                continue;
            }
            let method_name = handler_method_name(lines[method_start]).unwrap_or("<unknown>");
            findings.push(Finding {
                rule: Rule::HandlerAuthzSafety,
                path: path_text.to_string(),
                line: Some(method_start + 1),
                detail: format!(
                    "`{method_name}` must call require_authz*/authorize_* or carry an immediate // SAFETY: comment"
                ),
            });
        }

        index = impl_end.max(index + 1);
    }

    findings
}

fn is_service_impl(line: &str, service_traits: &BTreeSet<String>) -> bool {
    let Some(after_impl) = line.strip_prefix("impl ") else {
        return false;
    };
    let Some((before_for, _)) = after_impl.split_once(" for ") else {
        return false;
    };
    let Some(trait_name) = before_for
        .split_whitespace()
        .last()
        .map(|name| name.split('<').next().unwrap_or(name))
    else {
        return false;
    };
    service_traits.contains(trait_name)
}

fn service_impl_methods(
    lines: &[&str],
    impl_start: usize,
    scan_limit: usize,
) -> (usize, Vec<usize>) {
    let mut methods = Vec::new();
    let mut brace_depth = 0i32;
    let mut opened_impl = false;
    let mut line_index = impl_start;

    while line_index < scan_limit {
        let trimmed = lines[line_index].trim_start();
        if opened_impl && brace_depth == 1 && trimmed.starts_with("async fn ") {
            methods.push(line_index);
        }

        brace_depth += brace_delta(lines[line_index]);
        if brace_depth > 0 {
            opened_impl = true;
        }
        line_index += 1;
        if opened_impl && brace_depth <= 0 {
            break;
        }
    }

    (line_index, methods)
}

fn brace_delta(line: &str) -> i32 {
    let mut delta = 0;
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(character) = chars.next() {
        if !in_string && character == '/' && chars.peek() == Some(&'/') {
            break;
        }
        if character == '"' && !escaped {
            in_string = !in_string;
        }
        if !in_string {
            match character {
                '{' => delta += 1,
                '}' => delta -= 1,
                _ => {}
            }
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
    }

    delta
}

fn has_immediate_safety_comment(lines: &[&str], method_start: usize) -> bool {
    method_start
        .checked_sub(1)
        .and_then(|index| lines.get(index))
        .is_some_and(|line| line.trim_start().starts_with("// SAFETY:"))
}

fn has_authz_boundary(body: &str) -> bool {
    [
        "require_authz",
        "authorize_",
        "authorized_",
        "require_tenant_",
        "require_agent_",
        "require_grant_authority",
        "require_contact_scope",
        "require_contact_session_permission",
        "require_contact_agent_permission",
        "require_scim_admin",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

fn handler_method_name(line: &str) -> Option<&str> {
    line.trim_start()
        .strip_prefix("async fn ")
        .and_then(|rest| rest.split('(').next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

fn classify_line(line: &str) -> Option<Rule> {
    if contains_direct_sql(line) {
        return Some(Rule::DirectSql);
    }

    if line.contains("OrchestratorCtx::current_")
        || line.contains("OrchestratorCtx::current()")
        || line.contains(".embedding_provider()")
        || line.contains(".fga_client()")
        || line.contains(".graph_pool()")
        || line.contains(".graph_memory_retriever()")
        || line.contains(".lineage()")
        || line.contains(".session_store()")
        || line.contains(".skill_injector()")
        || line.contains(".tool_schemas()")
        || line.contains(".providers()")
        || line.contains(".auth_providers()")
        || line.contains(".provider_registry()")
        || line.contains(".tool_router()")
    {
        return Some(Rule::RuntimeContext);
    }

    None
}

fn contains_direct_sql(line: &str) -> bool {
    [
        "sqlx::query(",
        "sqlx::query!",
        "sqlx::query_as",
        "sqlx::query_scalar",
        "sqlx::query_file",
        "query!(",
        "query_as!(",
        "query_scalar!(",
        "query_file!(",
        "query_file_as!(",
        "query_file_scalar!(",
        "QueryBuilder::<",
    ]
    .iter()
    .any(|needle| line.contains(needle))
}

fn matching_allowance(rule: Rule, path: &str, line: &str) -> Option<usize> {
    ALLOWANCES.iter().position(|allowance| {
        allowance.rule == rule && allowance.path == path && line.contains(allowance.needle)
    })
}

fn collect_rust_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", root.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug)]
struct Finding {
    rule: Rule,
    path: String,
    line: Option<usize>,
    detail: String,
}

impl Finding {
    fn new(rule: Rule, path: String, line: usize, source: &str) -> Self {
        Self {
            rule,
            path,
            line: Some(line),
            detail: format!("unallowlisted source: {}", source.trim()),
        }
    }

    fn budget(rule: Rule, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            rule,
            path: path.into(),
            line: None,
            detail: detail.into(),
        }
    }

    fn exceeded_allowance(
        allowance: Allowance,
        actual_count: usize,
        line: usize,
        source: &str,
    ) -> Self {
        Self {
            rule: allowance.rule,
            path: allowance.path.to_string(),
            line: Some(line),
            detail: format!(
                "allowance exceeded for `{}`: expected {}, saw at least {}; reason: {}; source: {}",
                allowance.needle,
                allowance.expected_count,
                actual_count,
                allowance.reason,
                source.trim()
            ),
        }
    }

    fn stale_allowance(allowance: Allowance, actual_count: usize) -> Self {
        Self {
            rule: allowance.rule,
            path: allowance.path.to_string(),
            line: None,
            detail: format!(
                "stale allowance for `{}`: expected {}, saw {}; reason: {}",
                allowance.needle, allowance.expected_count, actual_count, allowance.reason
            ),
        }
    }

    fn display(&self) -> String {
        let location = match self.line {
            Some(line) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        };
        format!("{location}: {}: {}", self.rule, self.detail)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PackageGraph, ReverseDependencyBudget, Rule, SymbolBudget, classify_line,
        count_pub_use_exports, event_wildcard_match_arms, forbidden_dependency_findings,
        handler_authz_safety_findings, matching_allowance, restate_service_traits_from_source,
        reverse_dependency_budget_reports, scan_source, symbol_budget_finding,
    };

    #[test]
    fn classifies_direct_sql() {
        assert_eq!(
            classify_line("let rows = sqlx::query_scalar::<_, String>(\"SELECT 1\");"),
            Some(Rule::DirectSql)
        );
        assert_eq!(
            classify_line("let row = sqlx::query!(\"SELECT 1 as one\");"),
            Some(Rule::DirectSql)
        );
        assert_eq!(
            classify_line("let row = query_as!(Row, \"SELECT 1 as one\");"),
            Some(Rule::DirectSql)
        );
        assert_eq!(
            classify_line("let mut query = QueryBuilder::<Postgres>::new(\"SELECT 1\");"),
            Some(Rule::DirectSql)
        );
    }

    #[test]
    fn classifies_raw_context_access() {
        assert_eq!(
            classify_line("let pool = OrchestratorCtx::current_graph_pool();"),
            Some(Rule::RuntimeContext)
        );
        assert_eq!(
            classify_line("let store = runtime.session_store();"),
            Some(Rule::RuntimeContext)
        );
        assert_eq!(
            classify_line("let providers = runtime.provider_registry();"),
            Some(Rule::RuntimeContext)
        );
        assert_eq!(
            classify_line("let providers = runtime.auth_providers();"),
            Some(Rule::RuntimeContext)
        );
        assert_eq!(
            classify_line("let providers = OrchestratorCtx::current_provider_registry();"),
            Some(Rule::RuntimeContext)
        );
        assert_eq!(
            classify_line("let config = OrchestratorCtx::current_config().clone();"),
            Some(Rule::RuntimeContext)
        );
        assert_eq!(
            classify_line("let embedder = runtime.embedding_provider();"),
            Some(Rule::RuntimeContext)
        );
        assert_eq!(
            classify_line("OrchestratorCtx::current_lineage().record(json);"),
            Some(Rule::RuntimeContext)
        );
    }

    #[test]
    fn matches_counted_allowance_by_path_rule_and_needle() {
        let index = matching_allowance(
            Rule::DirectSql,
            "crates/moa-orchestrator/src/services/audit.rs",
            "let row = sqlx::query_as::<_, Row>(\"SELECT tenant_id\");",
        )
        .expect("audit SQL exception should be allowlisted");
        assert!(
            index < super::ALLOWANCES.len(),
            "allowance index should point into the allowlist"
        );
    }

    #[test]
    fn rejects_same_needle_on_unallowlisted_path() {
        assert_eq!(
            matching_allowance(
                Rule::DirectSql,
                "crates/moa-orchestrator/src/services/new_handler.rs",
                "let rows = sqlx::query(\"SELECT 1\");",
            ),
            None
        );
    }

    #[test]
    fn rejects_upward_dependency_on_orchestrator() {
        // Pins: domain crates cannot depend upward on the Restate adapter boundary.
        let graph = PackageGraph::for_tests(
            &["moa-core", "moa-orchestrator", "moa-providers"],
            &["moa-core", "moa-orchestrator", "moa-providers"],
            &[("moa-providers", "moa-orchestrator")],
        );

        let findings = forbidden_dependency_findings(&graph, super::FORBIDDEN_DEPENDENCY_RULES);

        assert_eq!(findings.len(), 1, "one upward dependency should fail");
        assert_eq!(findings[0].rule, Rule::ForbiddenDependency);
        assert!(
            findings[0]
                .detail
                .contains("moa-providers -> moa-orchestrator"),
            "finding should name the rejected edge"
        );
    }

    #[test]
    fn rejects_moa_core_fan_in_over_budget() {
        // Pins: new direct moa-core reverse dependencies require an intentional budget update.
        let graph = PackageGraph::for_tests(
            &["moa-brain", "moa-core", "moa-session"],
            &["moa-brain", "moa-core", "moa-session"],
            &[("moa-brain", "moa-core"), ("moa-session", "moa-core")],
        );
        let budgets = [ReverseDependencyBudget {
            package: "moa-core",
            max_direct: 1,
            max_transitive: 2,
            reason: "synthetic fan-in budget",
        }];

        let (reports, findings) = reverse_dependency_budget_reports(&graph, &budgets);

        assert_eq!(reports[0].direct_count, 2);
        assert_eq!(reports[0].transitive_count, 2);
        assert_eq!(findings.len(), 1, "direct fan-in over budget should fail");
        assert_eq!(findings[0].rule, Rule::ReverseDependencyBudget);
    }

    #[test]
    fn rejects_moa_core_re_export_budget_growth() {
        // Pins: the moa-core top-level re-export wall cannot grow silently.
        let source = r#"
pub use analytics::{CacheDailyMetric, SessionAnalyticsSummary};
pub use error::MoaError;
"#;
        let count = count_pub_use_exports(source);
        let budget = SymbolBudget {
            label: "synthetic moa-core exports",
            path: "crates/moa-core/src/lib.rs",
            max_count: 2,
            reason: "synthetic re-export budget",
        };

        let finding = symbol_budget_finding(budget, count)
            .expect("three re-exported symbols should exceed a budget of two");

        assert_eq!(count, 3);
        assert_eq!(finding.rule, Rule::SymbolBudget);
        assert!(
            finding.detail.contains("expected at most 2, saw 3"),
            "finding should include exact re-export counts"
        );
    }

    #[test]
    fn existing_orchestrator_allowances_are_counted_exactly() {
        // Pins: counted orchestrator exceptions consume their allowance and remain stale-proof.
        let index = matching_allowance(
            Rule::DirectSql,
            "crates/moa-orchestrator/src/services/audit.rs",
            "let row = sqlx::query_as::<_, Row>(\"SELECT tenant_id\");",
        )
        .expect("audit query_as allowance should exist");
        let mut allowance_uses = vec![0usize; super::ALLOWANCES.len()];
        let mut findings = Vec::new();
        let service_traits = std::collections::BTreeSet::new();

        scan_source(
            "crates/moa-orchestrator/src/services/audit.rs",
            r#"
let tenant = sqlx::query_as::<_, TenantRow>("SELECT tenant_id");
let payload = sqlx::query_as::<_, PayloadRow>("SELECT payload");
"#,
            &service_traits,
            &mut allowance_uses,
            &mut findings,
        );

        assert!(findings.is_empty(), "allowed audit SQL should not fail");
        assert_eq!(allowance_uses[index], 2, "audit SQL allowance count");
    }

    #[test]
    fn rejects_wildcard_event_match_arms_in_sensitive_consumers() {
        // Pins: sensitive Event consumers cannot hide new variants behind catch-all previews.
        let source = r#"
fn snippet(event: &Event) -> String {
    match event {
        Event::UserMessage { text, .. } => text.clone(),
        other => format!("{other:?}"),
    }
}

fn json(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(text.clone()),
        _ => value.clone(),
    }
}
"#;

        let arms = event_wildcard_match_arms(source);

        assert_eq!(arms.len(), 1);
        assert_eq!(arms[0].line, 5);
        assert!(
            arms[0].source.contains("other =>"),
            "finding should point at the wildcard Event arm"
        );
    }

    #[test]
    fn accepts_exhaustive_event_match_arms_in_sensitive_consumers() {
        // Pins: explicit Event variant arms are accepted even when the same file has non-Event wildcards.
        let source = r#"
fn snippet(event: &Event) -> String {
    match event {
        Event::UserMessage { text, .. } => text.clone(),
        Event::Warning { message } => message.clone(),
    }
}

fn json(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(text.clone()),
        _ => value.clone(),
    }
}
"#;

        assert!(event_wildcard_match_arms(source).is_empty());
    }

    #[test]
    fn restate_handler_without_authz_or_safety_is_flagged() {
        // Pins: a new Restate service handler cannot read or mutate caller-owned data without an explicit authz boundary marker.
        let source = r#"#[restate_sdk::service]
pub trait Example {
    async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
    async fn read(&self, _ctx: Context<'_>) -> Result<(), HandlerError> {
        Ok(())
    }
}
"#;
        let service_traits = restate_service_traits_from_source(source);
        let findings = handler_authz_safety_findings(
            "crates/moa-orchestrator/src/services/example.rs",
            source,
            &service_traits,
        );

        assert_eq!(
            findings.len(),
            1,
            "missing marker should produce one finding"
        );
        assert_eq!(findings[0].rule, Rule::HandlerAuthzSafety);
        assert_eq!(findings[0].line, Some(7));
    }

    #[test]
    fn restate_handler_with_immediate_safety_comment_is_allowed() {
        // Pins: intentionally internal or informational handlers document why resource authz is not applied.
        let source = r#"#[restate_sdk::service]
pub trait Example {
    async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
    #[tracing::instrument(skip(self, _ctx))]
    // SAFETY: informational status endpoint with no caller-owned data.
    async fn read(&self, _ctx: Context<'_>) -> Result<(), HandlerError> {
        Ok(())
    }
}
"#;
        let service_traits = restate_service_traits_from_source(source);
        let findings = handler_authz_safety_findings(
            "crates/moa-orchestrator/src/services/example.rs",
            source,
            &service_traits,
        );

        assert!(findings.is_empty(), "immediate SAFETY marker should pass");
    }

    #[test]
    fn restate_handler_with_visible_authz_helper_is_allowed() {
        // Pins: local authorization helper calls in handler bodies count as the behavior-boundary check.
        let source = r#"#[restate_sdk::service]
pub trait Example {
    async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
    async fn read(&self, ctx: Context<'_>) -> Result<(), HandlerError> {
        authorize_tenant(&ctx).await?;
        Ok(())
    }
}
"#;
        let service_traits = restate_service_traits_from_source(source);
        let findings = handler_authz_safety_findings(
            "crates/moa-orchestrator/src/services/example.rs",
            source,
            &service_traits,
        );

        assert!(findings.is_empty(), "visible authz helper should pass");
    }

    #[test]
    fn allowlist_reasons_are_not_empty() {
        for allowance in super::ALLOWANCES {
            assert!(
                !allowance.reason.trim().is_empty(),
                "allowlist entry for {} must carry a reason",
                allowance.path
            );
        }
    }

    #[test]
    fn allowlist_entries_are_unique() {
        let mut seen = std::collections::BTreeMap::new();
        for allowance in super::ALLOWANCES {
            let key = (allowance.rule, allowance.path, allowance.needle);
            let previous = seen.insert(key, allowance.expected_count);
            assert!(
                previous.is_none(),
                "duplicate allowlist entry for {} / {}",
                allowance.path,
                allowance.needle
            );
        }
    }
}
