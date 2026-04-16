//! `file_grep` tool implementation.

use std::ffi::OsStr;
use std::path::{Component, Path};
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use moa_core::{MoaError, Result, ToolContent, ToolOutput};
use regex::Regex;
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::{
    docker_file_read, docker_file_search, resolve_container_workspace_path,
};
use crate::tools::file_search::default_skipped_dirs;

const DEFAULT_GREP_PATH_GLOB: &str = "**/*";
const DEFAULT_GREP_MAX_MATCHES: usize = 200;
const MAX_GREP_MATCHES: usize = 1_000;
const MAX_LINE_PREVIEW_CHARS: usize = 240;

/// Executes the `file_grep` tool against a sandbox directory.
pub async fn execute(
    sandbox_dir: &Path,
    input: &str,
    extra_skips: &[String],
) -> Result<ToolOutput> {
    let params: FileGrepInput = serde_json::from_str(input)?;
    let search = GrepSearch::from_input(&params)?;
    let mut matches = Vec::new();
    let hit_limit =
        collect_matches(sandbox_dir, sandbox_dir, &search, extra_skips, &mut matches).await?;

    Ok(build_file_grep_output(
        matches,
        hit_limit,
        extra_skips,
        search.max_matches,
        Duration::default(),
    ))
}

/// Executes the `file_grep` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    extra_skips: &[String],
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: FileGrepInput = serde_json::from_str(input)?;
    let search = GrepSearch::from_input(&params)?;
    let candidate_paths = docker_file_search(
        container_id,
        &search.path_glob,
        workspace_root,
        timeout,
        hard_cancel_token,
    )
    .await?;

    let mut matches = Vec::new();
    for relative_path in candidate_paths {
        if matches.len() >= search.max_matches {
            break;
        }
        if should_skip_search_path(Path::new(&relative_path), extra_skips) {
            continue;
        }

        let absolute_path = resolve_container_workspace_path(workspace_root, &relative_path)?;
        let Ok(content) =
            docker_file_read(container_id, &absolute_path, timeout, hard_cancel_token).await
        else {
            continue;
        };
        collect_file_matches(
            &relative_path,
            &content,
            &search.regex,
            &mut matches,
            search.max_matches,
        );
    }

    let hit_limit = matches.len() >= search.max_matches;
    Ok(build_file_grep_output(
        matches,
        hit_limit,
        extra_skips,
        search.max_matches,
        Duration::default(),
    ))
}

async fn collect_matches(
    root: &Path,
    current: &Path,
    search: &GrepSearch,
    extra_skips: &[String],
    matches: &mut Vec<GrepMatch>,
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
            if should_skip_search_path(relative_path, extra_skips) {
                continue;
            }
            if Box::pin(collect_matches(root, &path, search, extra_skips, matches)).await? {
                return Ok(true);
            }
            continue;
        }
        if !file_type.is_file()
            || should_skip_search_path(relative_path, extra_skips)
            || !search.matcher.is_match(relative_path)
        {
            continue;
        }

        let Ok(content) = fs::read_to_string(&path).await else {
            continue;
        };
        collect_file_matches(
            &relative_path.display().to_string(),
            &content,
            &search.regex,
            matches,
            search.max_matches,
        );
        if matches.len() >= search.max_matches {
            return Ok(true);
        }
    }

    Ok(false)
}

fn collect_file_matches(
    display_path: &str,
    content: &str,
    regex: &Regex,
    matches: &mut Vec<GrepMatch>,
    max_matches: usize,
) {
    for (line_index, line) in content.lines().enumerate() {
        if matches.len() >= max_matches {
            break;
        }
        if regex.is_match(line) {
            matches.push(GrepMatch {
                path: display_path.to_string(),
                line: line_index + 1,
                text: truncate_line(line),
            });
        }
    }
}

fn build_file_grep_output(
    matches: Vec<GrepMatch>,
    hit_limit: bool,
    extra_skips: &[String],
    max_matches: usize,
    duration: Duration,
) -> ToolOutput {
    let skipped_directories = skipped_directory_names(extra_skips);
    let structured = serde_json::json!({
        "matches": matches.iter().map(|entry| serde_json::json!({
            "path": entry.path,
            "line": entry.line,
            "text": entry.text,
        })).collect::<Vec<_>>(),
        "truncated": hit_limit,
        "skipped_directories": skipped_directories.clone(),
    });

    let mut summary = if matches.is_empty() {
        "No matching lines found.".to_string()
    } else {
        matches
            .iter()
            .map(|entry| format!("{}:{}:{}", entry.path, entry.line, entry.text))
            .collect::<Vec<_>>()
            .join("\n")
    };

    if hit_limit {
        summary.push_str(&format!(
            "\n\n[search truncated at {} matches; narrow the pattern or path_glob]",
            max_matches
        ));
    }
    summary.push_str(&format!(
        "\n\n[skipped directories: {}]",
        skipped_directories.join(", ")
    ));

    ToolOutput {
        content: vec![ToolContent::Text { text: summary }],
        is_error: false,
        structured: Some(structured),
        duration,
    }
}

fn should_skip_search_path(path: &Path, extra_skips: &[String]) -> bool {
    path.components().any(|component| match component {
        Component::Normal(segment) => {
            default_skipped_dirs()
                .iter()
                .any(|ignored| segment == OsStr::new(ignored))
                || extra_skips
                    .iter()
                    .any(|ignored| segment == OsStr::new(ignored.as_str()))
        }
        _ => false,
    })
}

fn skipped_directory_names(extra_skips: &[String]) -> Vec<String> {
    let mut names = default_skipped_dirs()
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    for ignored in extra_skips {
        if !names.iter().any(|name| name == ignored) {
            names.push(ignored.clone());
        }
    }
    names
}

fn truncate_line(line: &str) -> String {
    let char_count = line.chars().count();
    if char_count <= MAX_LINE_PREVIEW_CHARS {
        return line.to_string();
    }

    line.chars()
        .take(MAX_LINE_PREVIEW_CHARS)
        .collect::<String>()
        + "..."
}

struct GrepSearch {
    path_glob: String,
    matcher: GlobMatcher,
    regex: Regex,
    max_matches: usize,
}

impl GrepSearch {
    fn from_input(input: &FileGrepInput) -> Result<Self> {
        let path_glob = input
            .path_glob
            .clone()
            .unwrap_or_else(|| DEFAULT_GREP_PATH_GLOB.to_string());
        let matcher = Glob::new(&path_glob)
            .map_err(|error| MoaError::ValidationError(error.to_string()))?
            .compile_matcher();

        let regex_pattern = if input.fixed_string.unwrap_or(false) {
            regex::escape(&input.pattern)
        } else {
            input.pattern.clone()
        };
        let regex = Regex::new(&regex_pattern)
            .map_err(|error| MoaError::ValidationError(error.to_string()))?;

        let max_matches = input
            .max_matches
            .unwrap_or(DEFAULT_GREP_MAX_MATCHES)
            .clamp(1, MAX_GREP_MATCHES);

        Ok(Self {
            path_glob,
            matcher,
            regex,
            max_matches,
        })
    }
}

#[derive(Debug, Deserialize)]
struct FileGrepInput {
    pattern: String,
    path_glob: Option<String>,
    fixed_string: Option<bool>,
    max_matches: Option<usize>,
}

#[derive(Debug)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;

    #[tokio::test]
    async fn file_grep_finds_matching_lines() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("server/core"))
            .await
            .expect("create dirs");
        fs::write(
            dir.path().join("server/core/views.py"),
            "class CallViewSet:\n    pass\n",
        )
        .await
        .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"pattern":"CallViewSet","path_glob":"server/**/*.py"}"#,
            &[],
        )
        .await
        .expect("grep");

        let text = output.to_text();
        assert!(text.contains("server/core/views.py:1:class CallViewSet:"));
    }

    #[tokio::test]
    async fn file_grep_respects_skip_directories() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".venv/lib"))
            .await
            .expect("create dirs");
        fs::create_dir_all(dir.path().join("server/core"))
            .await
            .expect("create dirs");
        fs::write(
            dir.path().join(".venv/lib/ignored.py"),
            "class CallViewSet:\n",
        )
        .await
        .expect("write file");
        fs::write(
            dir.path().join("server/core/views.py"),
            "class CallViewSet:\n",
        )
        .await
        .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"pattern":"CallViewSet","path_glob":"**/*.py"}"#,
            &[],
        )
        .await
        .expect("grep");

        let text = output.to_text();
        assert!(text.contains("server/core/views.py:1:class CallViewSet:"));
        assert!(!text.contains(".venv/lib/ignored.py"));
    }

    #[tokio::test]
    async fn file_grep_supports_fixed_string_matching() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("server/core"))
            .await
            .expect("create dirs");
        fs::write(
            dir.path().join("server/core/views.py"),
            "router.register(r\"calls\", views.CallViewSet, basename=\"calls\")\n",
        )
        .await
        .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"pattern":"router.register(r\"calls\", views.CallViewSet, basename=\"calls\")","path_glob":"server/**/*.py","fixed_string":true}"#,
            &[],
        )
        .await
        .expect("grep");

        assert!(output.to_text().contains("basename=\"calls\""));
    }
}
