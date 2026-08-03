//! Source-layout, direct-SQL, context-access, and event-consumer rules.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::report::{Finding, Rule};
use super::restate_rules::{
    brace_delta, collect_restate_service_traits, handler_authz_safety_findings,
};

pub(super) const SCAN_ROOTS: &[&str] = &[
    "crates/moa-orchestrator/src/objects",
    "crates/moa-orchestrator/src/services",
    "crates/moa-orchestrator/src/workflows",
];

#[derive(Debug, Clone, Copy)]
pub(super) struct Allowance {
    pub(super) rule: Rule,
    pub(super) path: &'static str,
    pub(super) needle: &'static str,
    pub(super) expected_count: usize,
    pub(super) reason: &'static str,
}

impl Allowance {
    pub(super) fn removal_task(self) -> Option<&'static str> {
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
pub(super) const ALLOWANCES: &[Allowance] = &[
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
];

const RELEASE_SERVING_TABLES: &[&str] = &[
    "moa.artifact_serving_pointer",
    "moa.artifact_activation_audit",
];
const SQL_WRITE_PREFIXES: &[&str] = &["insert into", "update", "delete from"];

pub(super) const SENSITIVE_EVENT_CONSUMERS: &[SensitiveEventConsumer] = &[
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
pub(super) struct SensitiveEventConsumer {
    pub(super) path: &'static str,
    pub(super) max_wildcard_event_match_arms: usize,
    pub(super) reason: &'static str,
}

pub(super) fn scan_sensitive_event_consumers(
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
pub(super) struct EventWildcardMatchArm {
    pub(super) line: usize,
    pub(super) source: String,
}

pub(super) fn event_wildcard_match_arms(source: &str) -> Vec<EventWildcardMatchArm> {
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

pub(super) fn scan_roots(roots: &[&str]) -> Result<Vec<Finding>> {
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
pub(super) fn scan_release_serving_writes(root: &Path) -> Result<Vec<Finding>> {
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

pub(super) fn scan_source(
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

pub(super) fn classify_line(line: &str) -> Option<Rule> {
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

pub(super) fn is_repository_code_path(path: &str) -> bool {
    let path = Path::new(path);
    path.file_name().and_then(|name| name.to_str()) == Some("repository.rs")
        || path.parent().is_some_and(|parent| {
            parent
                .components()
                .any(|part| part.as_os_str() == "repository")
        })
}

pub(super) fn matching_allowance(rule: Rule, path: &str, line: &str) -> Option<usize> {
    ALLOWANCES.iter().position(|allowance| {
        allowance.rule == rule && allowance.path == path && line.contains(allowance.needle)
    })
}

pub(super) fn collect_rust_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
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

pub(super) fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
