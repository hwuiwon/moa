//! Workspace, source-size, and public-symbol architecture budgets.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::graph::{
    FORBIDDEN_DEPENDENCY_RULES, PackageGraph, forbidden_dependency_findings, load_package_graph,
};
use super::report::{
    ArchitectureReport, Finding, LocBudgetReport, ReverseDependencyReport, Rule, SymbolBudgetReport,
};
use super::source_rules::{ALLOWANCES, SCAN_ROOTS, SENSITIVE_EVENT_CONSUMERS, collect_rust_files};

pub(super) const WORKSPACE_PACKAGE_COUNT_BUDGET: usize = 49;
pub(super) const WORKSPACE_DEFAULT_MEMBER_COUNT_BUDGET: usize = 46;
const MOA_CORE_ROOT_EXPORT_ALLOWLIST: &[&str] = &["MoaError", "Result", "WORKSPACE_ID"];
const REVERSE_DEPENDENCY_BUDGETS: &[ReverseDependencyBudget] = &[ReverseDependencyBudget {
    package: "moa-core",
    max_direct: 42,
    max_transitive: 44,
    reason: "architecture-policy ADRs 0008 and 0009 fold DLP into provider governance and Auth0 into its sole auth-provider owner, reducing the exact workspace ratchets to 49 packages, 46 default members, and moa-core fan-in to 42 direct and 44 transitive reverse dependencies; further growth requires a new decision record rather than a budget bump",
}];

const LOC_BUDGETS: &[LocBudget] = &[
    LocBudget {
        label: "moa-core Rust source",
        path: "crates/moa-core/src",
        scope: LocScope::RustTree,
        max_lines: 25_836,
        reason: "Unified Execute routing adds shared Respond/Execute/NeedsInput decisions, Inline/Durable strategies, classifier provenance and configuration, normalized planning audits, session events, and observability DTOs without rebuilding the moa-core root facade",
    },
    LocBudget {
        label: "public edge route ladder",
        path: "crates/moa-edge/src/routes.rs",
        scope: LocScope::File,
        max_lines: 1_749,
        reason: "Tasks 17-18 extract tenant-account routes and add durable purge admission/status orchestration; Task 27 makes its foundational imports explicit; tenant-operations MCP adds the authenticated edge-router composition seam",
    },
    LocBudget {
        label: "moa-config env overlay",
        path: "crates/moa-config/src/env_overlay/mod.rs",
        scope: LocScope::File,
        max_lines: 1_664,
        reason: "one flat MOA_* overlay owns every typed environment override in a single serde surface so unknown-key detection stays exhaustive; the cap holds at its pre-extraction value and further growth requires a new decision record",
    },
    LocBudget {
        label: "turn execution workflow",
        path: "crates/moa-orchestrator/src/workflows/turn_execution/mod.rs",
        scope: LocScope::File,
        max_lines: 1_589,
        reason: "Unified Execute routing adds one bounded classifier, normalized route audits, the Inline loop, one workflow-owned root Durable-upgrade control with exact evidence handoff, replay-safe Durable admission, amendment handling, and compact terminal synthesis within the workflow shell; the prompt-injection security circuit adds the coordinator owner outcomes (halt into the existing TurnFailed writer, suspend idling on the CoordinatorInput awakeable) at the tool-dispatch seam; further growth requires a new decision record",
    },
    LocBudget {
        label: "worker state types",
        path: "crates/moa-core/src/types/worker/state.rs",
        scope: LocScope::File,
        max_lines: 417,
        reason: "worker state and lifecycle DTOs stay isolated from command and tool-schema concerns; `WorkerInitialTask` owns the authenticated identity a child inherits from its parent, and the `request_input` round-trip now carries its exact owner (`WorkerInputTarget`/`WorkerInputRequest` plus the fenced `WorkerPendingInput`) so a clear or reply names one turn generation instead of a bare request id; further growth requires a new decision record",
    },
    LocBudget {
        label: "worker command types",
        path: "crates/moa-core/src/types/worker/commands.rs",
        scope: LocScope::File,
        max_lines: 352,
        reason: "Task 8 shares one typed Applied/Replayed/Conflict reply-delivery acknowledgement between worker and execution input flows while retaining the isolated worker command boundary",
    },
    LocBudget {
        label: "worker tool schemas",
        path: "crates/moa-core/src/types/worker/tool_schema.rs",
        scope: LocScope::File,
        max_lines: 789,
        reason: "Task 28 isolates model-facing delegation and child-report schemas from worker state",
    },
    LocBudget {
        label: "session handler shell",
        path: "crates/moa-orchestrator/src/objects/session/handlers.rs",
        scope: LocScope::File,
        max_lines: 400,
        reason: "the modularity refactor reduced the session handler shell to routing and shared helpers; behavior belongs in its lifecycle, turn, review, worker, progress, and execution-bridge modules",
    },
    LocBudget {
        label: "session state shell",
        path: "crates/moa-orchestrator/src/objects/session/state.rs",
        scope: LocScope::File,
        max_lines: 425,
        reason: "the modularity refactor split session state into lifecycle, persistence, input, resume, segment, worker, and execution projections",
    },
    LocBudget {
        label: "session turn handlers",
        path: "crates/moa-orchestrator/src/objects/session/handlers/turns.rs",
        scope: LocScope::File,
        max_lines: 500,
        reason: "turn admission, replies, outcomes, and resume behavior stay in their focused child modules instead of regrowing one session handler file",
    },
    LocBudget {
        label: "worker handler shell",
        path: "crates/moa-orchestrator/src/objects/worker/handlers.rs",
        scope: LocScope::File,
        max_lines: 300,
        reason: "the modularity refactor reduced the worker handler shell to shared dispatch while admission, turn, coordination, and cleanup remain separate",
    },
    LocBudget {
        label: "worker coordination handlers",
        path: "crates/moa-orchestrator/src/objects/worker/handlers/coordination.rs",
        scope: LocScope::File,
        max_lines: 700,
        reason: "worker coordination is the largest focused handler owner after splitting the former multi-thousand-line handler module",
    },
    LocBudget {
        label: "worker state shell",
        path: "crates/moa-orchestrator/src/objects/worker/state/mod.rs",
        scope: LocScope::File,
        max_lines: 250,
        reason: "worker lifecycle, coordination, storage, and result projections remain in focused state modules",
    },
    LocBudget {
        label: "worker lifecycle state",
        path: "crates/moa-orchestrator/src/objects/worker/state/lifecycle.rs",
        scope: LocScope::File,
        max_lines: 425,
        reason: "worker lifecycle is the largest production state owner after the state split and must not absorb coordination or storage again",
    },
    LocBudget {
        label: "execution service shell",
        path: "crates/moa-orchestrator/src/services/execution.rs",
        scope: LocScope::File,
        max_lines: 250,
        reason: "the execution Restate surface delegates to focused handlers, capability catalog, planning context, and support modules",
    },
    LocBudget {
        label: "execution service handlers",
        path: "crates/moa-orchestrator/src/services/execution/handlers.rs",
        scope: LocScope::File,
        max_lines: 1_500,
        reason: "transport handlers remain separate from capability-catalog construction, planning context, support, and tests",
    },
    LocBudget {
        label: "execution repository shell",
        path: "crates/moa-execution/src/repository/mod.rs",
        scope: LocScope::File,
        max_lines: 1_250,
        reason: "the execution repository shell coordinates focused run, task, transition, audit, outcome, terminal, row, and SQL modules",
    },
    LocBudget {
        label: "execution compiler validation",
        path: "crates/moa-execution/src/compiler/validation.rs",
        scope: LocScope::File,
        max_lines: 1_000,
        reason: "compiler validation is the largest focused compiler module after estimate, amendment, and test extraction",
    },
    LocBudget {
        label: "execution interpreter terminal decisions",
        path: "crates/moa-execution/src/interpreter/terminal.rs",
        scope: LocScope::File,
        max_lines: 450,
        reason: "terminal interpretation stays separate from projection, reservation, materialization, and aggregation",
    },
    LocBudget {
        label: "artifact validation shell",
        path: "crates/moa-artifacts/src/validation.rs",
        scope: LocScope::File,
        max_lines: 2_300,
        reason: "HTTP connector validation lives in its focused child module and must not regrow inside the shared artifact validator",
    },
    LocBudget {
        label: "HTTP connector artifact validation",
        path: "crates/moa-artifacts/src/validation/connectors.rs",
        scope: LocScope::File,
        max_lines: 550,
        reason: "the HTTP-only connector validator excludes deleted legacy and managed-runtime branches",
    },
    LocBudget {
        label: "knowledge service test-support shell",
        path: "crates/moa-orchestrator/tests/knowledge_service/support.rs",
        scope: LocScope::File,
        max_lines: 150,
        reason: "knowledge service fixtures are routed through focused service, sync, ingestion, provider, webhook, connector, credential, and repository owners",
    },
    LocBudget {
        label: "knowledge repository test fake",
        path: "crates/moa-orchestrator/tests/knowledge_service/support/repository.rs",
        scope: LocScope::File,
        max_lines: 1_400,
        reason: "the repository fake is the largest concrete knowledge fixture owner after splitting the former multi-thousand-line support file",
    },
    LocBudget {
        label: "knowledge ingestion test support",
        path: "crates/moa-orchestrator/tests/knowledge_service/support/ingestion.rs",
        scope: LocScope::File,
        max_lines: 750,
        reason: "knowledge ingestion fixtures stay separate from sync, provider, credential, connector, webhook, service, and repository support",
    },
    LocBudget {
        label: "migration DB harness shell",
        path: "crates/moa-migrations/tests/run_idempotency_db.rs",
        scope: LocScope::File,
        max_lines: 50,
        reason: "the migration DB lane remains one thin harness over protocol, purge, connector, knowledge, hand, execution-security, and learning-lineage behavior modules",
    },
    LocBudget {
        label: "connector migration behavior module",
        path: "crates/moa-migrations/tests/run_idempotency_db/connectors.rs",
        scope: LocScope::File,
        max_lines: 2_300,
        reason: "connector migration scenarios remain in their concrete owner instead of regrowing the shared migration harness",
    },
    LocBudget {
        label: "execution DB harness shell",
        path: "crates/moa-execution/tests/execution_db.rs",
        scope: LocScope::File,
        max_lines: 50,
        reason: "the execution DB lane remains one thin harness over planning, lifecycle, budget, outcome, cancellation, and support modules",
    },
    LocBudget {
        label: "execution lifecycle DB behavior module",
        path: "crates/moa-execution/tests/execution_db/scope_and_lifecycle_db.rs",
        scope: LocScope::File,
        max_lines: 1_500,
        reason: "execution scope and lifecycle scenarios remain in their concrete owner instead of regrowing the shared execution DB harness",
    },
    LocBudget {
        label: "connector Restate service shell",
        path: "crates/moa-orchestrator/src/services/connectors.rs",
        scope: LocScope::File,
        max_lines: 100,
        reason: "the connector service shell only declares focused authz, credential, definition, management, Restate, and wire modules",
    },
    LocBudget {
        label: "connector management handlers",
        path: "crates/moa-orchestrator/src/services/connectors/management.rs",
        scope: LocScope::File,
        max_lines: 900,
        reason: "connector management is the largest focused application owner after the Restate service split",
    },
    LocBudget {
        label: "connector Restate bindings",
        path: "crates/moa-orchestrator/src/services/connectors/restate.rs",
        scope: LocScope::File,
        max_lines: 500,
        reason: "connector Restate translation remains separate from management, credentials, definition resolution, authz, and wire conversion",
    },
    LocBudget {
        label: "connector repository shell",
        path: "crates/moa-connectors/src/repository.rs",
        scope: LocScope::File,
        max_lines: 400,
        reason: "the connector repository shell owns shared transactions and delegates lifecycle, managed-parent, invocation, use-grant, catalog-batch, and row mapping operations",
    },
    LocBudget {
        label: "connector lifecycle repository",
        path: "crates/moa-connectors/src/repository/lifecycle.rs",
        scope: LocScope::File,
        max_lines: 550,
        reason: "connection lifecycle persistence is the largest focused connector repository owner after the repository split",
    },
    LocBudget {
        label: "managed knowledge-parent repository",
        path: "crates/moa-connectors/src/repository/managed_parents.rs",
        scope: LocScope::File,
        max_lines: 500,
        reason: "closed Nango/Merge parent claims remain separate from public connector lifecycle and invocation persistence",
    },
];

const MOA_CORE_ROOT_PATH: &str = "crates/moa-core/src/lib.rs";
// `moa-core`'s root re-exports its types module, so the root-export count is only
// meaningful when that module is expanded alongside `lib.rs`.
const MOA_CORE_TYPES_MODULE_PATH: &str = "crates/moa-core/src/types/mod.rs";

const SYMBOL_BUDGETS: &[SymbolBudget] = &[SymbolBudget {
    label: "moa-core top-level pub use exports",
    path: MOA_CORE_ROOT_PATH,
    max_count: 3,
    reason: "Task 29 limits the crate root to universal error/result and workspace identity exports",
}];

#[derive(Debug, Clone, Copy)]
pub(super) struct ReverseDependencyBudget {
    pub(super) package: &'static str,
    pub(super) max_direct: usize,
    pub(super) max_transitive: usize,
    pub(super) reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct LocBudget {
    pub(super) label: &'static str,
    pub(super) path: &'static str,
    scope: LocScope,
    max_lines: usize,
    pub(super) reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
enum LocScope {
    File,
    RustTree,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SymbolBudget {
    pub(super) label: &'static str,
    pub(super) path: &'static str,
    pub(super) max_count: usize,
    pub(super) reason: &'static str,
}

/// One repository path the architecture policy is configured against, paired
/// with the label of the configuration entry that owns it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ConfiguredPath {
    pub(super) owner: String,
    pub(super) path: String,
}

impl ConfiguredPath {
    fn new(owner: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            path: path.into(),
        }
    }
}

// Every path any rule reads, labelled by the configuration entry that names it.
// Collected as a set so a path shared by several entries is validated once per
// distinct owner rather than once per occurrence.
pub(super) fn configured_paths() -> Vec<ConfiguredPath> {
    SCAN_ROOTS
        .iter()
        .map(|root| ConfiguredPath::new("orchestrator scan root", *root))
        .chain(ALLOWANCES.iter().map(|allowance| {
            ConfiguredPath::new(
                format!("{} allowance `{}`", allowance.rule, allowance.needle),
                allowance.path,
            )
        }))
        .chain(
            LOC_BUDGETS.iter().map(|budget| {
                ConfiguredPath::new(format!("{} LOC budget", budget.label), budget.path)
            }),
        )
        .chain(SYMBOL_BUDGETS.iter().map(|budget| {
            ConfiguredPath::new(format!("{} symbol budget", budget.label), budget.path)
        }))
        .chain(
            SENSITIVE_EVENT_CONSUMERS
                .iter()
                .map(|consumer| ConfiguredPath::new("sensitive Event consumer", consumer.path)),
        )
        .chain([ConfiguredPath::new(
            "moa-core types module expanded by the root symbol budget",
            MOA_CORE_TYPES_MODULE_PATH,
        )])
        .chain(
            crate::execution_trace_manifest::configured_paths()
                .into_iter()
                .map(|path| ConfiguredPath::new("execution trace manifest", path)),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

// Reports which configured paths are absent under `root`, so tests can point
// the same validation at a synthetic tree instead of the repository.
pub(super) fn missing_configured_paths(
    root: &Path,
    configured: &[ConfiguredPath],
) -> Vec<ConfiguredPath> {
    configured
        .iter()
        .filter(|entry| !root.join(&entry.path).exists())
        .cloned()
        .collect()
}

/// Fails before any rule runs when a configured path no longer exists.
///
/// A moved or deleted owner otherwise aborts the scan mid-run with an opaque
/// read error, so the remaining rules never report. Each missing entry names
/// the configuration label that owns it and the exact path it expects.
pub(super) fn validate_configured_paths(root: &Path) -> Result<()> {
    let missing = missing_configured_paths(root, &configured_paths());
    if missing.is_empty() {
        return Ok(());
    }

    bail!(
        "architecture policy references {} path(s) that do not exist; repoint the owning configuration entry:\n{}",
        missing.len(),
        missing
            .iter()
            .map(|entry| format!("  owner `{}` -> missing path `{}`", entry.owner, entry.path))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

pub(super) fn scan_architecture_budgets() -> Result<(ArchitectureReport, Vec<Finding>)> {
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
            dev_only_edges: graph.dev_only_edges(),
            forbidden_dependency_rule_count: FORBIDDEN_DEPENDENCY_RULES.len(),
        },
        findings,
    ))
}

pub(super) fn reverse_dependency_budget_reports(
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
        let count = if budget.path == MOA_CORE_ROOT_PATH {
            let types_path = root.join(MOA_CORE_TYPES_MODULE_PATH);
            let types_source = fs::read_to_string(&types_path)
                .with_context(|| format!("read {}", types_path.display()))?;
            count_moa_core_root_exports(&source, &types_source)
        } else {
            count_pub_use_exports(&source)
        };
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
        if budget.path == MOA_CORE_ROOT_PATH
            && let Some(finding) = moa_core_root_export_allowlist_finding(&source)
        {
            findings.push(finding);
        }
    }

    Ok((reports, findings))
}

pub(super) fn symbol_budget_finding(budget: SymbolBudget, count: usize) -> Option<Finding> {
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

pub(super) fn moa_core_root_export_allowlist_finding(source: &str) -> Option<Finding> {
    let (exports, has_wildcard) = pub_use_export_names(source);
    let expected = MOA_CORE_ROOT_EXPORT_ALLOWLIST
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();

    (has_wildcard || exports != expected).then(|| {
        Finding::budget(
            Rule::SymbolBudget,
            MOA_CORE_ROOT_PATH,
            format!(
                "moa-core root exports must exactly match {:?} with no wildcard; saw {:?}{}",
                expected,
                exports,
                if has_wildcard {
                    " and a wildcard export"
                } else {
                    ""
                }
            ),
        )
    })
}

fn pub_use_export_names(source: &str) -> (BTreeSet<String>, bool) {
    let mut exports = BTreeSet::new();
    let mut has_wildcard = false;
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
            if let Some(target) = pub_use_target(&statement) {
                collect_pub_use_leaf_names(target, &mut exports, &mut has_wildcard);
            }
            statement.clear();
            in_pub_use = false;
        }
    }

    (exports, has_wildcard)
}

fn collect_pub_use_leaf_names(
    target: &str,
    exports: &mut BTreeSet<String>,
    has_wildcard: &mut bool,
) {
    let target = target.trim();
    if target == "*" || target.ends_with("::*") {
        *has_wildcard = true;
        return;
    }

    if let (Some(open_brace), Some(close_brace)) = (target.find('{'), target.rfind('}')) {
        for item in split_top_level_comma_items(&target[open_brace + 1..close_brace]) {
            collect_pub_use_leaf_names(item, exports, has_wildcard);
        }
        return;
    }

    let leaf = target
        .split_once(" as ")
        .map_or(target, |(_source, alias)| alias)
        .rsplit("::")
        .next()
        .unwrap_or(target)
        .trim();
    if !leaf.is_empty() && leaf != "self" {
        exports.insert(leaf.to_string());
    }
}

fn split_top_level_comma_items(source: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (index, character) in source.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(source[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    items.push(source[start..].trim());
    items
}

pub(super) fn count_pub_use_exports(source: &str) -> usize {
    count_pub_use_exports_with_types(source, None)
}

pub(super) fn count_moa_core_root_exports(source: &str, types_source: &str) -> usize {
    count_pub_use_exports_with_types(source, Some(types_source))
}

fn count_pub_use_exports_with_types(source: &str, types_source: Option<&str>) -> usize {
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
            count += if pub_use_target(&statement) == Some("types::*") {
                types_source.map_or(1, count_pub_use_exports)
            } else {
                count_pub_use_statement_exports(&statement)
            };
            statement.clear();
            in_pub_use = false;
        }
    }

    count
}

fn pub_use_target(statement: &str) -> Option<&str> {
    statement
        .trim()
        .strip_prefix("pub use ")
        .map(str::trim)
        .map(|value| value.trim_end_matches(';').trim())
}

fn count_pub_use_statement_exports(statement: &str) -> usize {
    let Some(exports) = pub_use_target(statement) else {
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
