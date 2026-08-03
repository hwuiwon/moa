//! Architecture-check findings and human-readable reports.

use std::fmt;

use super::budgets::{WORKSPACE_DEFAULT_MEMBER_COUNT_BUDGET, WORKSPACE_PACKAGE_COUNT_BUDGET};
use super::source_rules::Allowance;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Rule {
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

#[derive(Debug)]
pub(super) struct ArchitectureReport {
    pub(super) workspace_package_count: usize,
    pub(super) default_member_count: usize,
    pub(super) reverse_dependencies: Vec<ReverseDependencyReport>,
    pub(super) loc_budgets: Vec<LocBudgetReport>,
    pub(super) symbol_budgets: Vec<SymbolBudgetReport>,
    pub(super) dev_only_edges: Vec<(String, String)>,
    pub(super) forbidden_dependency_rule_count: usize,
}

impl ArchitectureReport {
    pub(super) fn display(&self) -> String {
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
        lines.join("\n")
    }
}

#[derive(Debug)]
pub(super) struct ReverseDependencyReport {
    pub(super) package: &'static str,
    pub(super) direct_count: usize,
    pub(super) transitive_count: usize,
    pub(super) max_direct: usize,
    pub(super) max_transitive: usize,
    pub(super) reason: &'static str,
}

#[derive(Debug)]
pub(super) struct LocBudgetReport {
    pub(super) label: &'static str,
    pub(super) path: &'static str,
    pub(super) lines: usize,
    pub(super) max_lines: usize,
    pub(super) reason: &'static str,
}

#[derive(Debug)]
pub(super) struct SymbolBudgetReport {
    pub(super) label: &'static str,
    pub(super) path: &'static str,
    pub(super) count: usize,
    pub(super) max_count: usize,
    pub(super) reason: &'static str,
}

#[derive(Debug)]
pub(super) struct Finding {
    pub(super) rule: Rule,
    pub(super) path: String,
    pub(super) line: Option<usize>,
    pub(super) detail: String,
}

impl Finding {
    pub(super) fn new(rule: Rule, path: String, line: usize, source: &str) -> Self {
        Self {
            rule,
            path,
            line: Some(line),
            detail: format!("unallowlisted source: {}", source.trim()),
        }
    }

    pub(super) fn budget(rule: Rule, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            rule,
            path: path.into(),
            line: None,
            detail: detail.into(),
        }
    }

    pub(super) fn exceeded_allowance(
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

    pub(super) fn stale_allowance(allowance: Allowance, actual_count: usize) -> Self {
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

    pub(super) fn display(&self) -> String {
        let location = match self.line {
            Some(line) => format!("{}:{line}", self.path),
            None => self.path.clone(),
        };
        format!("{location}: {}: {}", self.rule, self.detail)
    }
}
