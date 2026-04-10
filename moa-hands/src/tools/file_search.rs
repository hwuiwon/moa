//! `file_search` tool implementation.

use std::path::Path;
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use moa_core::{Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::docker_file_search;

/// Executes the `file_search` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileSearchInput = serde_json::from_str(input)?;
    let matcher = Glob::new(&params.pattern)
        .map_err(|error| moa_core::MoaError::ValidationError(error.to_string()))?
        .compile_matcher();
    let mut matches = Vec::new();
    collect_matches(sandbox_dir, sandbox_dir, &matcher, &mut matches).await?;
    matches.sort();

    let data = serde_json::Value::Array(
        matches
            .iter()
            .map(|path| serde_json::json!({ "path": path }))
            .collect(),
    );
    let summary = if matches.is_empty() {
        "No matching files found.".to_string()
    } else {
        matches.join("\n")
    };

    Ok(ToolOutput::json(summary, data, Duration::default()))
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
    let matches = docker_file_search(
        container_id,
        &params.pattern,
        workspace_root,
        timeout,
        hard_cancel_token,
    )
    .await?;
    let data = serde_json::Value::Array(
        matches
            .iter()
            .map(|path| serde_json::json!({ "path": path }))
            .collect(),
    );
    let summary = if matches.is_empty() {
        "No matching files found.".to_string()
    } else {
        matches.join("\n")
    };

    Ok(ToolOutput::json(summary, data, Duration::default()))
}

async fn collect_matches(
    root: &Path,
    current: &Path,
    matcher: &GlobMatcher,
    matches: &mut Vec<String>,
) -> Result<()> {
    let mut entries = fs::read_dir(current).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            Box::pin(collect_matches(root, &path, matcher, matches)).await?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        if let Ok(relative_path) = path.strip_prefix(root)
            && matcher.is_match(relative_path)
        {
            matches.push(relative_path.display().to_string());
        }
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct FileSearchInput {
    pattern: String,
}
