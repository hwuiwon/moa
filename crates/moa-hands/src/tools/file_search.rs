//! `file_search` tool implementation.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::Component;
use std::path::Path;
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use moa_core::{error::Result, types::tools::ToolContent, types::tools::ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::docker_file_search;

const MAX_FILE_SEARCH_MATCHES: usize = 1_000;
const MAX_FILE_SEARCH_SUMMARY_MATCHES: usize = 200;
const SKIPPED_SEARCH_DIRS: &[&str] = &[
    // Version control
    ".git",
    ".svn",
    ".hg",
    // JavaScript / TypeScript
    "node_modules",
    ".next",
    ".nuxt",
    ".turbo",
    "dist",
    "build",
    ".output",
    // Rust
    "target",
    // Python
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".eggs",
    // Java / Kotlin
    ".gradle",
    ".mvn",
    // Go / PHP
    "vendor",
    // Ruby
    ".bundle",
    // .NET
    "obj",
    // iOS
    "Pods",
    // IDE / editor
    ".idea",
    ".vscode",
    ".direnv",
    // General caches
    ".cache",
    "coverage",
    "htmlcov",
    ".coverage",
    "__generated__",
];

/// Returns the default skipped directory names for documentation and prompt generation.
pub fn default_skipped_dirs() -> &'static [&'static str] {
    SKIPPED_SEARCH_DIRS
}

/// Executes the `file_search` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileSearchInput = serde_json::from_str(input)?;
    let matcher = Glob::new(&params.pattern)
        .map_err(|error| moa_core::error::MoaError::ValidationError(error.to_string()))?
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
        .filter(|path| !should_skip_search_path_static(Path::new(path)))
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
    let mut entries = match fs::read_dir(current).await {
        Ok(entries) => entries,
        Err(error) if should_ignore_search_io_error(&error) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) if should_ignore_search_io_error(&error) => break,
            Err(error) => return Err(error.into()),
        };
        let path = entry.path();
        let file_type = match entry.file_type().await {
            Ok(file_type) => file_type,
            Err(error) if should_ignore_search_io_error(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        let relative_path = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            if should_skip_search_path_static(relative_path) {
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
        if should_skip_search_path_static(relative_path) {
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

fn should_ignore_search_io_error(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound)
}

/// Returns whether a path should be skipped during repository searches.
pub fn should_skip_search_path_static(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(segment) => SKIPPED_SEARCH_DIRS
            .iter()
            .any(|ignored| segment == OsStr::new(ignored)),
        _ => false,
    })
}

fn build_file_search_output(
    mut matches: Vec<String>,
    hit_limit: bool,
    duration: Duration,
) -> ToolOutput {
    matches.sort();
    let skipped_directories = default_skipped_dirs();

    let structured_matches = matches
        .iter()
        .map(|path| serde_json::json!({ "path": path }))
        .collect::<Vec<_>>();
    let structured = serde_json::json!({
        "matches": structured_matches,
        "truncated": hit_limit,
        "skipped_directories": skipped_directories,
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
            skipped_directories.join(", ")
        ));
        summary
    };

    ToolOutput {
        content: vec![ToolContent::Text { text: summary }],
        is_error: false,
        structured: Some(structured),
        duration,
        truncated: false,
        original_output_tokens: None,
        artifact: None,
    }
}

#[derive(Debug, Deserialize)]
struct FileSearchInput {
    pattern: String,
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::path::Path;

    use serde_json::json;
    use tempfile::tempdir;
    use tokio::fs;

    use super::{
        default_skipped_dirs, execute, should_ignore_search_io_error,
        should_skip_search_path_static,
    };

    #[test]
    fn skips_python_venv_directory() {
        let path = Path::new(".venv/lib/python3.12/site-packages/requests/api.py");
        assert!(should_skip_search_path_static(path));
    }

    #[test]
    fn skips_pycache_directory() {
        let path = Path::new("server/core/__pycache__/views.cpython-312.pyc");
        assert!(should_skip_search_path_static(path));
    }

    #[test]
    fn does_not_skip_normal_source_files() {
        let path = Path::new("server/core/views.py");
        assert!(!should_skip_search_path_static(path));
    }

    #[test]
    fn skips_gradle_directory() {
        let path = Path::new(".gradle/caches/modules-2/files-2.1/com.google/guava.jar");
        assert!(should_skip_search_path_static(path));
    }

    #[test]
    fn skips_vendor_directory() {
        let path = Path::new("vendor/github.com/pkg/errors/errors.go");
        assert!(should_skip_search_path_static(path));
    }

    #[test]
    fn default_skipped_dirs_includes_polyglot_ecosystem_directories() {
        let skipped = default_skipped_dirs();
        assert!(skipped.contains(&".venv"));
        assert!(skipped.contains(&".gradle"));
        assert!(skipped.contains(&"vendor"));
    }

    #[test]
    fn missing_entries_are_ignored_during_search() {
        let error = std::io::Error::new(ErrorKind::NotFound, "disappeared");
        assert!(should_ignore_search_io_error(&error));
    }

    #[tokio::test]
    async fn execute_skips_python_virtualenv_matches() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".venv/lib"))
            .await
            .unwrap();
        fs::create_dir_all(dir.path().join("server/core"))
            .await
            .unwrap();
        fs::write(dir.path().join(".venv/lib/ignored.py"), "print('ignore')")
            .await
            .unwrap();
        fs::write(dir.path().join("server/core/views.py"), "print('keep')")
            .await
            .unwrap();

        let output = execute(dir.path(), &json!({ "pattern": "**/*.py" }).to_string())
            .await
            .unwrap();
        let rendered = output.to_text();

        assert!(rendered.contains("server/core/views.py"));
        assert!(!rendered.contains(".venv/lib/ignored.py"));
    }
}
