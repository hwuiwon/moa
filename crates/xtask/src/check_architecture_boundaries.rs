//! `xtask check-architecture-boundaries` command implementation.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const SCAN_ROOTS: &[&str] = &[
    "crates/moa-orchestrator/src/objects",
    "crates/moa-orchestrator/src/services",
    "crates/moa-orchestrator/src/workflows",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    DirectSql,
    RuntimeContext,
}

impl fmt::Display for Rule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectSql => formatter.write_str("direct SQL in handler/workflow code"),
            Self::RuntimeContext => formatter.write_str("raw OrchestratorCtx dependency access"),
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
        "crates/moa-orchestrator/src/objects/workspace.rs",
        "OrchestratorCtx::current()",
        1,
        "Workspace VO still owns a narrow memory-summary read pending a workspace repository seam"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/objects/workspace.rs",
        ".graph_pool()",
        1,
        "Workspace VO memory-summary read currently obtains the graph pool from grouped deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/objects/workspace.rs",
        "OrchestratorCtx::current_session_store",
        1,
        "Workspace VO workflow listing still reads through the session-store seam"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/objects/workspace.rs",
        "sqlx::query_scalar",
        1,
        "Workspace VO memory summary has one direct graph-node count pending repository extraction"
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
        "crates/moa-orchestrator/src/services/analytics.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Analytics summary endpoint still owns direct analytical read models"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/analytics.rs",
        "OrchestratorCtx::current_session_store",
        6,
        "Analytics service reads session projections through the current store seam"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/analytics.rs",
        "sqlx::query(",
        4,
        "Analytics read-model SQL remains in the handler pending a dedicated analytics repository"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/analytics.rs",
        "QueryBuilder::<",
        1,
        "Analytics learning-candidate list uses dynamic filters pending a dedicated query object"
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
        7,
        "Contact service constructs the initial in-process contact repository operations"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/contacts.rs",
        "OrchestratorCtx::current_session_store",
        4,
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
        DirectSql,
        "crates/moa-orchestrator/src/services/contacts.rs",
        "sqlx::query(",
        13,
        "Initial contact repository SQL remains local to the Contacts service before repository extraction"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/contacts.rs",
        "sqlx::query_scalar",
        4,
        "Initial contact lookup SQL remains local to the Contacts service before repository extraction"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/eval.rs",
        "OrchestratorCtx::current()",
        1,
        "Eval service still combines provider registry and analytics persistence"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/eval.rs",
        ".graph_pool()",
        1,
        "Eval service reads the pool from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/eval.rs",
        "OrchestratorCtx::current_config",
        2,
        "Internal eval runner still reads model and database config from runtime config accessors"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/eval.rs",
        "QueryBuilder::<",
        1,
        "Eval dataset persistence builds a multi-row insert pending repository extraction"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/eval.rs",
        "OrchestratorCtx::current_graph_pool",
        4,
        "Eval dataset management remains a temporary service-owned read model"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/eval.rs",
        "sqlx::query(",
        3,
        "Eval dataset SQL remains temporary until an eval repository is extracted"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/eval.rs",
        "sqlx::query_scalar",
        1,
        "Eval dataset insertion SQL remains temporary until an eval repository is extracted"
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
        ".session_store()",
        1,
        "Experiment run start still dispatches session-backed workflows"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/experiments.rs",
        "OrchestratorCtx::current_graph_pool",
        7,
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
        "Graph-memory maintenance scans workspaces until a maintenance repository owns it"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/learning_review.rs",
        "OrchestratorCtx::current()",
        1,
        "Learning review handler needs grouped store/provider deps for regression gating"
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
        ".session_store()",
        1,
        "Learning review handler passes the session store to the extracted review store"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/learning_review.rs",
        "OrchestratorCtx::current_session_store",
        2,
        "Learning review handlers still construct review stores per request"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/lineage_admin.rs",
        "OrchestratorCtx::current_graph_pool",
        5,
        "Lineage admin adapter keeps explicit authz/read-only transaction setup"
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
        "crates/moa-orchestrator/src/services/memory.rs",
        "OrchestratorCtx::current()",
        1,
        "Memory handler needs grouped pool access until the memory app seam is extracted"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory.rs",
        ".graph_pool()",
        1,
        "Memory handler reads the graph pool from grouped runtime deps"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory.rs",
        "OrchestratorCtx::current_graph_pool",
        1,
        "Memory handler still constructs graph stores directly for the legacy memory endpoint"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory.rs",
        "OrchestratorCtx::current_config",
        1,
        "Memory debug lineage endpoint still reads lineage config before emitting diagnostics"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/memory.rs",
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
        "crates/moa-orchestrator/src/services/privacy.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Privacy adapter keeps token/vault/export orchestration while erasure moved to owning crates"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/privacy.rs",
        "sqlx::query(",
        2,
        "Privacy export sets the auditor role locally and resolves contact subjects before controlled export reads"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/privacy.rs",
        "sqlx::query_scalar",
        8,
        "Privacy DSAR export read-model and linked-contact SQL remains in the adapter pending an export repository"
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
        1,
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
        1,
        "Experiment target execution still reads experiment definitions from workflow code"
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
        1,
        "Experiment trial target execution still reads trial state from workflow code"
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
        "crates/moa-orchestrator/src/workflows/skill_learning.rs",
        ".session_store()",
        1,
        "Skill-learning workflow still writes proposals through the session-store seam"
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
        "OrchestratorCtx::current_lineage",
        1,
        "TurnExecution still obtains the lineage handle while generation lineage is emitted inline"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/workflows/turn_execution.rs",
        "OrchestratorCtx::current_config",
        4,
        "TurnExecution still reads session and resolution config from the runtime singleton"
    ),
];

/// Runs the architecture-boundary scanner.
pub fn run() -> Result<()> {
    let findings = scan_roots(SCAN_ROOTS)?;
    if findings.is_empty() {
        println!(
            "architecture boundary checks clean: {} allowlisted exception groups checked",
            ALLOWANCES.len()
        );
        return Ok(());
    }

    bail!(
        "architecture boundary violations detected:\n{}",
        findings
            .iter()
            .map(Finding::display)
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn scan_roots(roots: &[&str]) -> Result<Vec<Finding>> {
    let mut files = Vec::new();
    for root in roots {
        collect_rust_files(Path::new(root), &mut files)?;
    }
    files.sort();

    let mut findings = Vec::new();
    let mut allowance_uses = vec![0usize; ALLOWANCES.len()];
    for path in files {
        scan_file(&path, &mut allowance_uses, &mut findings)?;
    }
    for (index, allowance) in ALLOWANCES.iter().enumerate() {
        let used = allowance_uses[index];
        if used != allowance.expected_count {
            findings.push(Finding::stale_allowance(*allowance, used));
        }
    }

    Ok(findings)
}

fn scan_file(path: &Path, allowance_uses: &mut [usize], findings: &mut Vec<Finding>) -> Result<()> {
    if path.file_name().and_then(|name| name.to_str()) == Some("tests.rs") {
        return Ok(());
    }

    let path_text = normalize_path(path);
    let body = fs::read_to_string(path).with_context(|| format!("read {path_text}"))?;
    for (line_index, line) in body.lines().enumerate() {
        let Some(rule) = classify_line(line) else {
            continue;
        };
        let Some(allowance_index) = matching_allowance(rule, &path_text, line) else {
            findings.push(Finding::new(rule, path_text.clone(), line_index + 1, line));
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
    Ok(())
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
    use super::{Rule, classify_line, matching_allowance};

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
