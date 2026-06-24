//! `xtask check-architecture-boundaries` command implementation.

use std::collections::BTreeSet;
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
    HandlerAuthzSafety,
    RuntimeContext,
}

impl fmt::Display for Rule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectSql => formatter.write_str("direct SQL in handler/workflow code"),
            Self::HandlerAuthzSafety => {
                formatter.write_str("Restate handler without authz or SAFETY marker")
            }
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
        8,
        "Contact service constructs the initial in-process contact repository operations"
    ),
    allow!(
        RuntimeContext,
        "crates/moa-orchestrator/src/services/contacts.rs",
        "OrchestratorCtx::current_session_store",
        5,
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
        ".session_store()",
        1,
        "Experiment run start still dispatches session-backed workflows"
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
        ".graph_pool()",
        1,
        "Learning review acceptance passes the runtime pool into the extracted skill promotion flow"
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
        "Memory handler still constructs graph stores directly for the old memory endpoint"
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
        "crates/moa-orchestrator/src/services/privacy/mod.rs",
        "OrchestratorCtx::current_graph_pool",
        2,
        "Privacy adapter keeps token/vault/export orchestration while erasure moved to owning crates"
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
        6,
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
    if path_text.starts_with("crates/moa-orchestrator/src/services/") {
        findings.extend(handler_authz_safety_findings(
            &path_text,
            &body,
            service_traits,
        ));
    }
    Ok(())
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
        Rule, classify_line, handler_authz_safety_findings, matching_allowance,
        restate_service_traits_from_source,
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
