//! `file_search` tool implementation.

use std::path::Path;
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use moa_core::{Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;

/// Executes the `file_search` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileSearchInput = serde_json::from_str(input)?;
    let matcher = Glob::new(&params.pattern)
        .map_err(|error| moa_core::MoaError::ValidationError(error.to_string()))?
        .compile_matcher();
    let mut matches = Vec::new();
    collect_matches(sandbox_dir, sandbox_dir, &matcher, &mut matches).await?;
    matches.sort();

    Ok(ToolOutput {
        stdout: matches.join("\n"),
        stderr: String::new(),
        exit_code: 0,
        duration: Duration::default(),
    })
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
