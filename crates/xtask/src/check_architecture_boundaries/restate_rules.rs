//! Restate service discovery and handler authorization-boundary rules.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::report::{Finding, Rule};
use super::source_rules::normalize_path;

pub(super) fn collect_restate_service_traits(files: &[PathBuf]) -> Result<BTreeSet<String>> {
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

pub(super) fn restate_service_traits_from_source(source: &str) -> BTreeSet<String> {
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

pub(super) fn handler_authz_safety_findings(
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

pub(super) fn brace_delta(line: &str) -> i32 {
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
        "journal_context_authz",
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
