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
    ExecutionTraceManifest,
    ForbiddenDependency,
    HandlerAuthzSafety,
    LocBudget,
    RuntimeContext,
    ReverseDependencyBudget,
    ReleaseServingWriteBoundary,
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
            Self::ExecutionTraceManifest => {
                formatter.write_str("execution trace propagation manifest")
            }
            Self::ForbiddenDependency => formatter.write_str("forbidden dependency direction"),
            Self::HandlerAuthzSafety => {
                formatter.write_str("Restate handler without authz or SAFETY marker")
            }
            Self::LocBudget => formatter.write_str("LOC budget"),
            Self::RuntimeContext => formatter.write_str("raw OrchestratorCtx dependency access"),
            Self::ReverseDependencyBudget => formatter.write_str("reverse dependency budget"),
            Self::ReleaseServingWriteBoundary => {
                formatter.write_str("raw release-serving table write outside the database seam")
            }
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

impl Allowance {
    fn removal_task(self) -> Option<&'static str> {
        match self.path {
            "crates/moa-orchestrator/src/services/agents.rs"
            | "crates/moa-orchestrator/src/services/tenants.rs" => Some("Task 2"),
            "crates/moa-orchestrator/src/services/api_keys.rs"
            | "crates/moa-orchestrator/src/services/experiments.rs" => Some("Task 3"),
            path if path.starts_with("crates/moa-orchestrator/src/objects/") => Some("Task 14"),
            path if path.starts_with("crates/moa-orchestrator/src/services/") => Some("Task 15"),
            path if path.starts_with("crates/moa-orchestrator/src/workflows/") => Some("Task 16"),
            _ => None,
        }
    }
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
        DirectSql,
        "crates/moa-orchestrator/src/objects/tenant.rs",
        "sqlx::query_scalar",
        1,
        "Tenant VO memory summary has one direct graph-node count pending repository extraction"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/agent_definitions.rs",
        "sqlx::query(",
        4,
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
        DirectSql,
        "crates/moa-orchestrator/src/services/authz_admin.rs",
        "sqlx::query_scalar",
        1,
        "Authz admin still resolves one API-key ownership record inline"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/knowledge/inspect.rs",
        "sqlx::query(",
        1,
        "Knowledge query-trace inspection reads lineage diagnostics until an analytics repository owns it"
    ),
    allow!(
        DirectSql,
        "crates/moa-orchestrator/src/services/execution.rs",
        "sqlx::query(",
        4,
        "Task 9 keeps permanent external-template admission reserve, CAS, and replay reads beside the admission coordinator pending a dedicated repository seam"
    ),
];

// The workspace is deliberately split by category owner: core runtime, memory
// and learning, agents/artifacts/experiments, tools and providers, auth and
// lineage, and eval/dev tooling. These two numbers record that accepted split
// exactly as ratified by architecture-policy ADR 0005 after `moa-connectors`
// became the canonical connection-domain owner. Growing either requires a new
// decision record, not a silent bump.
const WORKSPACE_PACKAGE_COUNT_BUDGET: usize = 52;
const WORKSPACE_DEFAULT_MEMBER_COUNT_BUDGET: usize = 49;
const MOA_CORE_ROOT_EXPORT_ALLOWLIST: &[&str] = &["MoaError", "Result", "WORKSPACE_ID"];
const RELEASE_SERVING_TABLES: &[&str] = &[
    "moa.artifact_serving_pointer",
    "moa.artifact_activation_audit",
];
const SQL_WRITE_PREFIXES: &[&str] = &["insert into", "update", "delete from"];

const REVERSE_DEPENDENCY_BUDGETS: &[ReverseDependencyBudget] = &[ReverseDependencyBudget {
    package: "moa-core",
    max_direct: 44,
    max_transitive: 47,
    reason: "architecture-policy ADR 0005 adds moa-connectors as the canonical connection category owner; it consumes moa-core IDs and errors, raising the accepted fan-in to 44 direct and 47 transitive reverse dependencies; further growth requires a new decision record rather than a budget bump",
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

const NON_DOMAIN_ORCHESTRATOR_DEPENDENTS: &[&str] = &[
    "moa-orchestrator",
    "moa-edge",
    "moa-loadtest",
    "moa-fga-bootstrap",
    "xtask",
    "workspace-hack",
];

const FORBIDDEN_DEPENDENCY_RULES: &[ForbiddenDependencyRule] = &[
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-core"),
        target: DependencySelector::Prefix("moa-memory-"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps memory-owned graph/vector/PII/ingest types out of moa-core",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::WorkspaceExcept(NON_DOMAIN_ORCHESTRATOR_DEPENDENTS),
        target: DependencySelector::Exact("moa-orchestrator"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps moa-orchestrator as the Restate transport/workflow/composition boundary",
    },
];

const FORBIDDEN_DEPENDENCY_ALLOWANCES: &[DependencyEdgeAllowance] = &[DependencyEdgeAllowance {
    source: "moa-test-support",
    target: "moa-orchestrator",
    kind: DependencyKind::Dev,
    reason: "test-only fixture composition may depend on the Restate adapter; production moa-test-support code may not",
}];

const SENSITIVE_EVENT_CONSUMERS: &[SensitiveEventConsumer] = &[
    SensitiveEventConsumer {
        path: "crates/moa-session/src/store/session_store.rs",
        max_wildcard_event_match_arms: 1,
        reason: "Task 21 makes the split session persistence consumers exhaustive while preserving the current single wildcard ratchet",
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
    edge_kinds: &'static [DependencyKind],
    reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyKind {
    NormalBuild,
    Dev,
}

impl fmt::Display for DependencyKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NormalBuild => formatter.write_str("normal/build"),
            Self::Dev => formatter.write_str("dev"),
        }
    }
}

const ALL_DEPENDENCY_KINDS: &[DependencyKind] = &[DependencyKind::NormalBuild, DependencyKind::Dev];

#[derive(Debug, Clone, Copy)]
struct DependencyEdgeAllowance {
    source: &'static str,
    target: &'static str,
    kind: DependencyKind,
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
    normal_build_dependencies: BTreeMap<String, BTreeSet<String>>,
    dev_dependencies: BTreeMap<String, BTreeSet<String>>,
}

impl PackageGraph {
    fn package_count(&self) -> usize {
        self.workspace_members.len()
    }

    fn default_member_count(&self) -> usize {
        self.default_members.len()
    }

    fn direct_reverse_dependencies(&self, package: &str) -> BTreeSet<String> {
        self.normal_build_dependencies
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
            .normal_build_dependencies
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
            if let Some(dependencies) = self.normal_build_dependencies.get(&candidate) {
                stack.extend(dependencies.iter().cloned());
            }
        }

        false
    }

    fn dependencies(&self, kind: DependencyKind) -> &BTreeMap<String, BTreeSet<String>> {
        match kind {
            DependencyKind::NormalBuild => &self.normal_build_dependencies,
            DependencyKind::Dev => &self.dev_dependencies,
        }
    }

    fn dev_only_edges(&self) -> Vec<(String, String)> {
        self.dev_dependencies
            .iter()
            .flat_map(|(source, dependencies)| {
                dependencies.iter().filter_map(move |target| {
                    let is_normal_build = self
                        .normal_build_dependencies
                        .get(source)
                        .is_some_and(|normal_build| normal_build.contains(target));
                    (!is_normal_build).then(|| (source.clone(), target.clone()))
                })
            })
            .collect()
    }

    #[cfg(test)]
    fn for_tests(packages: &[&str], default_members: &[&str], edges: &[(&str, &str)]) -> Self {
        Self::for_tests_with_kinds(packages, default_members, edges, &[])
    }

    #[cfg(test)]
    fn for_tests_with_kinds(
        packages: &[&str],
        default_members: &[&str],
        normal_build_edges: &[(&str, &str)],
        dev_edges: &[(&str, &str)],
    ) -> Self {
        let workspace_members = packages
            .iter()
            .map(|package| (*package).to_string())
            .collect::<BTreeSet<_>>();
        let default_members = default_members
            .iter()
            .map(|package| (*package).to_string())
            .collect::<BTreeSet<_>>();
        let mut normal_build_dependencies = workspace_members
            .iter()
            .map(|package| (package.clone(), BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        let mut dev_dependencies = normal_build_dependencies.clone();

        for (source, target) in normal_build_edges {
            normal_build_dependencies
                .entry((*source).to_string())
                .or_default()
                .insert((*target).to_string());
        }
        for (source, target) in dev_edges {
            dev_dependencies
                .entry((*source).to_string())
                .or_default()
                .insert((*target).to_string());
        }

        Self {
            workspace_members,
            default_members,
            normal_build_dependencies,
            dev_dependencies,
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
    dev_only_edges: Vec<(String, String)>,
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
            "dev-only workspace dependency edges: {}",
            if self.dev_only_edges.is_empty() {
                "none".to_string()
            } else {
                self.dev_only_edges
                    .iter()
                    .map(|(source, target)| format!("{source} -> {target}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        lines.push(format!(
            "forbidden dependency direction rules checked: {}",
            self.forbidden_dependency_rule_count
        ));
        for allowance in FORBIDDEN_DEPENDENCY_ALLOWANCES {
            lines.push(format!(
                "exact {} dependency allowance: {} -> {}; reason: {}",
                allowance.kind, allowance.source, allowance.target, allowance.reason
            ));
        }
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

/// One repository path the architecture policy is configured against, paired
/// with the label of the configuration entry that owns it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ConfiguredPath {
    owner: String,
    path: String,
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
fn configured_paths() -> Vec<ConfiguredPath> {
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
fn missing_configured_paths(root: &Path, configured: &[ConfiguredPath]) -> Vec<ConfiguredPath> {
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
fn validate_configured_paths(root: &Path) -> Result<()> {
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

/// Runs the architecture-boundary scanner.
pub fn run() -> Result<()> {
    validate_configured_paths(Path::new("."))?;
    let mut findings = scan_roots(SCAN_ROOTS)?;
    findings.extend(scan_release_serving_writes(Path::new("crates"))?);
    findings.extend(
        crate::execution_trace_manifest::audit(Path::new("."))?
            .into_iter()
            .map(|diagnostic| {
                Finding::budget(
                    Rule::ExecutionTraceManifest,
                    diagnostic.path(),
                    diagnostic.detail().to_string(),
                )
            }),
    );
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
            dev_only_edges: graph.dev_only_edges(),
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
    let mut normal_build_dependencies = BTreeMap::new();
    let mut dev_dependencies = BTreeMap::new();
    for package_name in &workspace_members {
        let Some(package) = package_values.get(package_name) else {
            bail!("workspace package `{package_name}` missing from cargo metadata package list");
        };
        let package_dependencies = package
            .get("dependencies")
            .and_then(Value::as_array)
            .with_context(|| format!("package `{package_name}` missing dependencies array"))?;
        let mut package_normal_build_dependencies = BTreeSet::new();
        let mut package_dev_dependencies = BTreeSet::new();
        for dependency in package_dependencies {
            let dependency_name = value_string_field(dependency, "name")?;
            if !workspace_members.contains(dependency_name) {
                continue;
            }
            match dependency.get("kind").and_then(Value::as_str) {
                None | Some("normal" | "build") => {
                    package_normal_build_dependencies.insert(dependency_name.to_string());
                }
                Some("dev") => {
                    package_dev_dependencies.insert(dependency_name.to_string());
                }
                Some(kind) => bail!(
                    "package `{package_name}` dependency `{dependency_name}` has unsupported Cargo dependency kind `{kind}`"
                ),
            }
        }
        normal_build_dependencies.insert(package_name.clone(), package_normal_build_dependencies);
        dev_dependencies.insert(package_name.clone(), package_dev_dependencies);
    }

    Ok(PackageGraph {
        workspace_members,
        default_members,
        normal_build_dependencies,
        dev_dependencies,
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

fn moa_core_root_export_allowlist_finding(source: &str) -> Option<Finding> {
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

fn count_pub_use_exports(source: &str) -> usize {
    count_pub_use_exports_with_types(source, None)
}

fn count_moa_core_root_exports(source: &str, types_source: &str) -> usize {
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

fn forbidden_dependency_findings(
    graph: &PackageGraph,
    rules: &[ForbiddenDependencyRule],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for kind in [DependencyKind::NormalBuild, DependencyKind::Dev] {
        for (source, dependencies) in graph.dependencies(kind) {
            for target in dependencies {
                for rule in rules {
                    if !rule.edge_kinds.contains(&kind)
                        || !rule.source.matches(source, graph)
                        || !rule.target.matches(target, graph)
                    {
                        continue;
                    }
                    if FORBIDDEN_DEPENDENCY_ALLOWANCES.iter().any(|allowance| {
                        allowance.source == source
                            && allowance.target == target
                            && allowance.kind == kind
                    }) {
                        break;
                    }
                    findings.push(Finding::budget(
                        Rule::ForbiddenDependency,
                        "Cargo metadata",
                        format!(
                            "forbidden {kind} workspace dependency `{source} -> {target}`; reason: {}",
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

/// Refuses raw production writes to the two release-control tables.
///
/// Reads remain available to serving and replay repositories. All writes must
/// cross the checked release-control `SECURITY DEFINER` functions, so a new Rust
/// call site cannot silently regain direct pointer or audit DML.
fn scan_release_serving_writes(root: &Path) -> Result<Vec<Finding>> {
    let mut files = Vec::new();
    collect_rust_files(root, &mut files)?;
    files.sort();

    let mut findings = Vec::new();
    for path in files {
        let path_text = normalize_path(&path);
        if path_text.split('/').any(|component| component == "tests") {
            continue;
        }
        let body = fs::read_to_string(&path).with_context(|| format!("read {path_text}"))?;
        // Inline test modules deliberately probe the denied raw-DML path. They
        // are not production call sites and remain below the conventional final
        // cfg(test) boundary.
        let production = body.split("#[cfg(test)]").next().unwrap_or(&body);
        let continuation_free = production.replace("\\\r\n", " ").replace("\\\n", " ");
        let normalized = continuation_free
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for table in RELEASE_SERVING_TABLES {
            for prefix in SQL_WRITE_PREFIXES {
                let forbidden = format!("{prefix} {table}");
                if normalized.contains(&forbidden) {
                    findings.push(Finding::budget(
                        Rule::ReleaseServingWriteBoundary,
                        path_text.clone(),
                        format!("`{forbidden}` bypasses the checked release transition functions"),
                    ));
                }
            }
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
        if rule == Rule::DirectSql && is_repository_code_path(path_text) {
            continue;
        }
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
    let authz_wrappers = local_authz_wrapper_names(&lines, scan_limit);
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
            if has_immediate_safety_comment(&lines, method_start)
                || has_authz_boundary(&body)
                || calls_local_authz_wrapper(&body, &authz_wrappers)
            {
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
    // Walk upward across the item's contiguous comment/attribute block so a
    // multi-line `// SAFETY:` rationale is still recognized. A `// SAFETY:`
    // marker whose explanation spans several lines leaves a comment
    // continuation (not the marker) directly above `async fn`.
    let mut index = method_start;
    while index > 0 {
        index -= 1;
        let trimmed = lines[index].trim_start();
        if trimmed.starts_with("// SAFETY:") {
            return true;
        }
        if trimmed.starts_with("//") || trimmed.starts_with("#[") || trimmed.starts_with("#!") {
            continue;
        }
        break;
    }
    false
}

/// Returns the file-local free functions whose own body performs an authz check.
///
/// A handler may delegate its check to a wrapper that adds domain conditions on top of
/// `require_authz*` (scoping the request to the caller's own tenant before demanding an
/// admin relation, say). Those wrappers are *resolved and read* rather than trusted by
/// name: the allowlist below never has to grow for a same-file helper, and a helper that
/// stops checking authz stops satisfying its callers on the next run.
fn local_authz_wrapper_names(lines: &[&str], scan_limit: usize) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut index = 0;
    while index < scan_limit {
        let Some(name) = top_level_fn_name(lines[index]) else {
            index += 1;
            continue;
        };
        // Rustfmt closes a top-level item with a lone `}` in column zero.
        let mut end = index + 1;
        while end < scan_limit && lines[end] != "}" {
            end += 1;
        }
        if has_authz_boundary(&lines[index..end.min(scan_limit)].join("\n")) {
            names.insert(name.to_string());
        }
        index = end + 1;
    }
    names
}

/// Returns the name of the free function declared at column zero on `line`.
fn top_level_fn_name(line: &str) -> Option<&str> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let declaration = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub(super) "))
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);
    let declaration = declaration.strip_prefix("async ").unwrap_or(declaration);
    declaration
        .strip_prefix("fn ")
        .and_then(|rest| rest.split(['(', '<']).next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// Returns whether a handler body calls one of the resolved authz wrappers.
fn calls_local_authz_wrapper(body: &str, authz_wrappers: &BTreeSet<String>) -> bool {
    authz_wrappers
        .iter()
        .any(|wrapper| body.contains(&format!("{wrapper}(")))
}

/// Returns whether a body performs an authz check itself.
///
/// These names are trusted without being read, so the list stays short and covers only
/// helpers defined outside the file being scanned. A wrapper that lives beside its
/// callers is resolved by [`local_authz_wrapper_names`] instead — adding such a name here
/// would let the handler keep passing after the wrapper stopped checking anything.
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

fn is_repository_code_path(path: &str) -> bool {
    let path = Path::new(path);
    path.file_name().and_then(|name| name.to_str()) == Some("repository.rs")
        || path.parent().is_some_and(|parent| {
            parent
                .components()
                .any(|part| part.as_os_str() == "repository")
        })
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
                "allowance exceeded for `{}`: expected {}, saw at least {}; reason: {}; removal: {}; source: {}",
                allowance.needle,
                allowance.expected_count,
                actual_count,
                allowance.reason,
                allowance.removal_task().unwrap_or("unassigned"),
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
                "stale allowance for `{}`: expected {}, saw {}; reason: {}; removal: {}",
                allowance.needle,
                allowance.expected_count,
                actual_count,
                allowance.reason,
                allowance.removal_task().unwrap_or("unassigned")
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
    use std::path::{Path, PathBuf};

    use super::{
        DependencyKind, PackageGraph, ReverseDependencyBudget, Rule, SymbolBudget, classify_line,
        configured_paths, count_moa_core_root_exports, count_pub_use_exports,
        event_wildcard_match_arms, forbidden_dependency_findings, handler_authz_safety_findings,
        is_repository_code_path, matching_allowance, missing_configured_paths,
        moa_core_root_export_allowlist_finding, parse_package_graph,
        restate_service_traits_from_source, reverse_dependency_budget_reports,
        scan_release_serving_writes, scan_source, symbol_budget_finding, validate_configured_paths,
    };

    const ENV_OVERLAY_OWNER: &str = "moa-config env overlay LOC budget";
    const ENV_OVERLAY_PATH: &str = "crates/moa-config/src/env_overlay/mod.rs";

    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .expect("workspace root should resolve from the xtask manifest directory")
    }

    #[test]
    fn release_serving_tables_are_writeable_only_through_the_database_seam() {
        // Pins: a production raw write is rejected even when SQL is split across
        // a Rust line continuation, while an inline negative test may still
        // exercise PostgreSQL's permission denial.
        let root = tempfile::TempDir::new().expect("temp dir");
        let source = root.path().join("owner.rs");
        std::fs::write(
            &source,
            r#"
fn bypass() {
    let _ = "UPDATE \
        moa.artifact_serving_pointer SET revision_uid = gen_random_uuid()";
}

#[cfg(test)]
mod tests {
    const DENIED: &str = "DELETE FROM moa.artifact_activation_audit";
}
"#,
        )
        .expect("write scanner fixture");

        let findings = scan_release_serving_writes(root.path()).expect("scan fixture");
        assert_eq!(findings.len(), 1, "raw production pointer write must fail");
        assert_eq!(findings[0].rule, Rule::ReleaseServingWriteBoundary);
    }

    #[test]
    fn configured_paths_name_their_owner_and_exist_in_the_real_tree() {
        // Pins: every path the architecture policy is configured against exists,
        // and the env-overlay LOC budget owner points at its current moa-config
        // owner rather than the removed moa-core location.
        let configured = configured_paths();

        let env_overlay = configured
            .iter()
            .find(|entry| entry.owner == ENV_OVERLAY_OWNER)
            .unwrap_or_else(|| {
                panic!("configured paths should include an owner named `{ENV_OVERLAY_OWNER}`")
            });
        assert_eq!(
            env_overlay.path, ENV_OVERLAY_PATH,
            "the env-overlay LOC budget must be owned by moa-config"
        );

        let missing = missing_configured_paths(&repository_root(), &configured);
        assert!(
            missing.is_empty(),
            "configured architecture paths must exist; missing: {missing:?}"
        );
    }

    #[test]
    fn missing_configured_path_reports_its_owner_and_exact_path() {
        // Pins: a configured owner whose file was moved or deleted fails the
        // pre-scan with both the owner label and the exact path, instead of
        // aborting a later rule with an opaque read error.
        let root = tempfile::TempDir::new().expect("temp dir");
        let configured = configured_paths();
        for entry in &configured {
            if entry.path == ENV_OVERLAY_PATH {
                continue;
            }
            std::fs::create_dir_all(root.path().join(&entry.path))
                .expect("materialize configured path");
        }

        let missing = missing_configured_paths(root.path(), &configured);
        assert_eq!(
            missing.len(),
            1,
            "only the removed env-overlay owner should be missing; saw {missing:?}"
        );
        assert_eq!(missing[0].owner, ENV_OVERLAY_OWNER);
        assert_eq!(missing[0].path, ENV_OVERLAY_PATH);

        let error = validate_configured_paths(root.path())
            .expect_err("a missing configured path must fail the pre-scan")
            .to_string();
        assert!(
            error.contains(ENV_OVERLAY_OWNER),
            "error must name the configured owner; got {error}"
        );
        assert!(
            error.contains(ENV_OVERLAY_PATH),
            "error must name the exact missing path; got {error}"
        );
    }

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
    fn repository_paths_are_classified_by_exact_file_or_directory_component() {
        // Pins: repositories may own SQL, while similarly named handlers remain scanned.
        assert!(is_repository_code_path(
            "crates/moa-orchestrator/src/services/privacy/repository.rs"
        ));
        assert!(is_repository_code_path(
            "crates/moa-orchestrator/src/services/privacy/repository/erase.rs"
        ));
        assert!(!is_repository_code_path(
            "crates/moa-orchestrator/src/services/privacy/repository_helpers.rs"
        ));
        assert!(!is_repository_code_path(
            "crates/moa-orchestrator/src/services/privacy/my_repository/erase.rs"
        ));

        let service_traits = std::collections::BTreeSet::new();
        let mut allowance_uses = vec![0usize; super::ALLOWANCES.len()];
        let mut repository_findings = Vec::new();
        scan_source(
            "crates/moa-orchestrator/src/services/privacy/repository/erase.rs",
            "let row = sqlx::query(\"SELECT 1\");",
            &service_traits,
            &mut allowance_uses,
            &mut repository_findings,
        );
        assert!(repository_findings.is_empty());

        let mut helper_findings = Vec::new();
        scan_source(
            "crates/moa-orchestrator/src/services/privacy/repository_helpers.rs",
            "let row = sqlx::query(\"SELECT 1\");",
            &service_traits,
            &mut allowance_uses,
            &mut helper_findings,
        );
        assert_eq!(helper_findings.len(), 1);
        assert_eq!(helper_findings[0].rule, Rule::DirectSql);
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
    fn dependency_kind_fixture_separates_production_and_dev_edges() {
        // Pins: production reverse budgets exclude dev-only workspace dependencies.
        let metadata = br#"{
            "packages": [
                {"name":"moa-core","id":"core","dependencies":[]},
                {"name":"moa-brain","id":"brain","dependencies":[{"name":"moa-core","kind":null}]},
                {"name":"moa-edge","id":"edge","dependencies":[{"name":"moa-core","kind":"build"}]},
                {"name":"moa-devtool","id":"devtool","dependencies":[{"name":"moa-core","kind":"dev"}]}
            ],
            "workspace_members":["core","brain","edge","devtool"],
            "workspace_default_members":["core","brain","edge","devtool"]
        }"#;

        let graph = parse_package_graph(metadata).expect("fixture metadata should parse");

        assert_eq!(
            graph.direct_reverse_dependencies("moa-core"),
            ["moa-brain".to_string(), "moa-edge".to_string()].into()
        );
        assert_eq!(
            graph.dev_only_edges(),
            vec![("moa-devtool".to_string(), "moa-core".to_string())]
        );
        assert!(
            graph
                .dependencies(DependencyKind::Dev)
                .get("moa-devtool")
                .is_some_and(|dependencies| dependencies.contains("moa-core"))
        );
    }

    #[test]
    fn test_support_orchestrator_allowance_is_dev_only() {
        // Pins: test fixture composition cannot become a production dependency direction.
        let dev_graph = PackageGraph::for_tests_with_kinds(
            &["moa-test-support", "moa-orchestrator"],
            &["moa-test-support", "moa-orchestrator"],
            &[],
            &[("moa-test-support", "moa-orchestrator")],
        );
        assert!(
            forbidden_dependency_findings(&dev_graph, super::FORBIDDEN_DEPENDENCY_RULES).is_empty()
        );

        let production_graph = PackageGraph::for_tests(
            &["moa-test-support", "moa-orchestrator"],
            &["moa-test-support", "moa-orchestrator"],
            &[("moa-test-support", "moa-orchestrator")],
        );
        let findings =
            forbidden_dependency_findings(&production_graph, super::FORBIDDEN_DEPENDENCY_RULES);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("normal/build"));
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
    fn moa_core_root_export_allowlist_accepts_only_universal_symbols() {
        // Pins: the final root facade contains exactly the three documented universal symbols.
        let root_source = r#"
pub use error::{MoaError, Result};
pub use workspace::WORKSPACE_ID;
"#;

        assert_eq!(count_moa_core_root_exports(root_source, ""), 3);
        assert!(moa_core_root_export_allowlist_finding(root_source).is_none());
    }

    #[test]
    fn moa_core_root_export_allowlist_rejects_wildcards() {
        // Pins: wildcard exports cannot silently rebuild a flattened facade.
        let root_source = r#"
pub use error::{MoaError, Result};
pub use types::*;
pub use workspace::WORKSPACE_ID;
"#;

        let finding = moa_core_root_export_allowlist_finding(root_source)
            .expect("a root wildcard must violate the exact allowlist");
        assert!(finding.detail.contains("wildcard export"));
    }

    #[test]
    fn moa_core_root_export_allowlist_rejects_same_count_substitution() {
        // Pins: an equal-sized replacement cannot evade the semantic allowlist.
        let root_source = r#"
pub use error::{MoaError, Result};
pub use events::Event;
"#;

        let finding = moa_core_root_export_allowlist_finding(root_source)
            .expect("Event is not a universal root export");
        assert!(finding.detail.contains("Event"));
    }

    #[test]
    fn ordinary_wildcard_export_counts_as_one_without_known_module_expansion() {
        // Pins: semantic expansion is limited to moa-core's known types module.
        assert_eq!(count_pub_use_exports("pub use generated::*;"), 1);
    }

    #[test]
    fn existing_orchestrator_allowances_are_counted_exactly() {
        // Pins: counted orchestrator exceptions consume their allowance and remain stale-proof.
        let index = matching_allowance(
            Rule::DirectSql,
            "crates/moa-orchestrator/src/objects/tenant.rs",
            "sqlx::query_scalar",
        )
        .expect("tenant direct-SQL allowance should exist");
        let mut allowance_uses = vec![0usize; super::ALLOWANCES.len()];
        let mut findings = Vec::new();
        let service_traits = std::collections::BTreeSet::new();

        scan_source(
            "crates/moa-orchestrator/src/objects/tenant.rs",
            "sqlx::query_scalar(\"SELECT COUNT(*)\")",
            &service_traits,
            &mut allowance_uses,
            &mut findings,
        );

        assert!(
            findings.is_empty(),
            "one exact direct-SQL allowance should not fail"
        );
        assert_eq!(
            allowance_uses[index], 1,
            "tenant direct-SQL allowance count"
        );
    }

    #[test]
    fn exact_count_allowance_rejects_the_next_matching_use() {
        // Pins: a counted exception cannot silently grow within an allowed file.
        let mut allowance_uses = vec![0usize; super::ALLOWANCES.len()];
        let mut findings = Vec::new();
        let service_traits = std::collections::BTreeSet::new();

        scan_source(
            "crates/moa-orchestrator/src/objects/tenant.rs",
            &"sqlx::query_scalar(\"SELECT COUNT(*)\");\n".repeat(2),
            &service_traits,
            &mut allowance_uses,
            &mut findings,
        );

        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("expected 1, saw at least 2"));
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
    fn restate_handler_with_multiline_safety_comment_is_allowed() {
        // Pins: a `// SAFETY:` rationale that spans several comment lines above
        // `async fn` is recognized even though a continuation line, not the
        // marker itself, sits directly above the handler signature.
        let source = r#"#[restate_sdk::service]
pub trait Example {
    async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
    #[tracing::instrument(skip(self, _ctx))]
    // SAFETY: internal teardown dispatched by the owning VO's own cleanup path.
    // It reclaims only that owner's own scope and reads no caller-owned data back.
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

        assert!(
            findings.is_empty(),
            "multi-line SAFETY marker should pass; got {findings:?}"
        );
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
    fn same_file_wrapper_is_resolved_and_read_rather_than_trusted_by_name() {
        // Pins: a handler delegating to a wrapper defined in the same file passes only
        // when that wrapper's own body checks authz. Accepting any helper whose name
        // merely looks authoritative would turn this rule into a rubber stamp — the
        // second half of this test is the one that matters.
        let checked = r#"#[restate_sdk::service]
pub trait Example {
    async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
    async fn read(&self, ctx: Context<'_>) -> Result<(), HandlerError> {
        require_rebuild_authority(&ctx).await?;
        Ok(())
    }
}

async fn require_rebuild_authority(ctx: &Context<'_>) -> Result<(), HandlerError> {
    require_authz_with_delegation(ctx, ObjectType::Tenant, Relation::Admin).await
}
"#;
        let findings = handler_authz_safety_findings(
            "crates/moa-orchestrator/src/services/example.rs",
            checked,
            &restate_service_traits_from_source(checked),
        );
        assert!(
            findings.is_empty(),
            "a wrapper that really checks authz satisfies its callers; got {findings:?}"
        );

        let unchecked = checked.replace(
            "    require_authz_with_delegation(ctx, ObjectType::Tenant, Relation::Admin).await",
            "    Ok(())",
        );
        let findings = handler_authz_safety_findings(
            "crates/moa-orchestrator/src/services/example.rs",
            &unchecked,
            &restate_service_traits_from_source(&unchecked),
        );
        assert_eq!(
            findings.len(),
            1,
            "an authoritative-sounding wrapper that checks nothing must not clear its \
             callers; got {findings:?}"
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
            assert!(
                allowance.removal_task().is_some(),
                "allowlist entry for {} must name a removal task",
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
