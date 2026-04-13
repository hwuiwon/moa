//! `file_search` tool implementation.

use std::path::Component;
use std::path::Path;
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use moa_core::{Result, ToolContent, ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::docker_file_search;

const MAX_FILE_SEARCH_MATCHES: usize = 1_000;
const MAX_FILE_SEARCH_SUMMARY_MATCHES: usize = 200;
const SKIPPED_SEARCH_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".next",
    ".turbo",
    "dist",
    "build",
    ".direnv",
];

/// Executes the `file_search` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileSearchInput = serde_json::from_str(input)?;
    let matcher = Glob::new(&params.pattern)
        .map_err(|error| moa_core::MoaError::ValidationError(error.to_string()))?
        .compile_matcher();
    let mut matches = Vec::new();
    let hit_limit = collect_matches(sandbox_dir, sandbox_dir, &matcher, &mut matches).await?;
    Ok(build_file_search_output(
        matches,
        hit_limit,
        Duration::default(),
    ))
}

/// Executes the `file_search` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: FileSearchInput = serde_json::from_str(input)?;
    let mut matches = docker_file_search(
        container_id,
        &params.pattern,
        workspace_root,
        timeout,
        hard_cancel_token,
    )
    .await?;
    matches = matches
        .into_iter()
        .filter(|path| !should_skip_search_path(Path::new(path)))
        .collect::<Vec<_>>();
    let hit_limit = matches.len() > MAX_FILE_SEARCH_MATCHES;
    matches.truncate(MAX_FILE_SEARCH_MATCHES);

    Ok(build_file_search_output(
        matches,
        hit_limit,
        Duration::default(),
    ))
}

async fn collect_matches(
    root: &Path,
    current: &Path,
    matcher: &GlobMatcher,
    matches: &mut Vec<String>,
) -> Result<bool> {
    let mut entries = fs::read_dir(current).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        let relative_path = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            if should_skip_search_path(relative_path) {
                continue;
            }
            if Box::pin(collect_matches(root, &path, matcher, matches)).await? {
                return Ok(true);
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if should_skip_search_path(relative_path) {
            continue;
        }

        if matcher.is_match(relative_path) {
            matches.push(relative_path.display().to_string());
            if matches.len() >= MAX_FILE_SEARCH_MATCHES {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn should_skip_search_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(segment) => SKIPPED_SEARCH_DIRS
            .iter()
            .any(|ignored| segment == std::ffi::OsStr::new(ignored)),
        _ => false,
    })
}

fn build_file_search_output(
    mut matches: Vec<String>,
    hit_limit: bool,
    duration: Duration,
) -> ToolOutput {
    matches.sort();

    let structured_matches = matches
        .iter()
        .map(|path| serde_json::json!({ "path": path }))
        .collect::<Vec<_>>();
    let structured = serde_json::json!({
        "matches": structured_matches,
        "truncated": hit_limit,
        "skipped_directories": SKIPPED_SEARCH_DIRS,
    });

    let summary = if matches.is_empty() {
        "No matching files found.".to_string()
    } else {
        let mut summary = matches
            .iter()
            .take(MAX_FILE_SEARCH_SUMMARY_MATCHES)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if matches.len() > MAX_FILE_SEARCH_SUMMARY_MATCHES {
            summary.push_str(&format!(
                "\n\n[showing first {} of {} matches]",
                MAX_FILE_SEARCH_SUMMARY_MATCHES,
                matches.len()
            ));
        }
        if hit_limit {
            summary.push_str(&format!(
                "\n\n[search truncated at {} matches; narrow the pattern or search a subdirectory]",
                MAX_FILE_SEARCH_MATCHES
            ));
        }
        summary.push_str(&format!(
            "\n\n[skipped directories: {}]",
            SKIPPED_SEARCH_DIRS.join(", ")
        ));
        summary
    };

    ToolOutput {
        content: vec![ToolContent::Text { text: summary }],
        is_error: false,
        structured: Some(structured),
        duration,
    }
}

#[derive(Debug, Deserialize)]
struct FileSearchInput {
    pattern: String,
}
