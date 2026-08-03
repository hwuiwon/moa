//! `xtask check-architecture-boundaries` command implementation.

mod budgets;
mod graph;
mod report;
mod restate_rules;
mod source_rules;
#[cfg(test)]
mod tests;

use std::path::Path;

use anyhow::{Result, bail};

use self::budgets::{scan_architecture_budgets, validate_configured_paths};
use self::report::{Finding, Rule};
use self::source_rules::{
    ALLOWANCES, SCAN_ROOTS, SENSITIVE_EVENT_CONSUMERS, scan_release_serving_writes, scan_roots,
    scan_sensitive_event_consumers,
};

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
