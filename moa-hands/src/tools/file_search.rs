//! `file_search` tool implementation.

use std::ffi::OsStr;
use std::io::ErrorKind;
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

/// Loads additional skip directory names from a `.moaignore` file in the workspace root.
pub async fn load_moaignore(workspace_root: &Path) -> Vec<String> {
    let moaignore_path = workspace_root.join(".moaignore");
    match fs::read_to_string(moaignore_path).await {
        Ok(content) => content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Executes the `file_search` tool against a sandbox directory.
pub async fn execute(
    sandbox_dir: &Path,
    input: &str,
    extra_skips: &[String],
) -> Result<ToolOutput> {
    let params: FileSearchInput = serde_json::from_str(input)?;
    let matcher = Glob::new(&params.pattern)
        .map_err(|error| moa_core::MoaError::ValidationError(error.to_string()))?
        .compile_matcher();
    let mut matches = Vec::new();
    let hit_limit = collect_matches(
        sandbox_dir,
        sandbox_dir,
        &matcher,
        extra_skips,
        &mut matches,
    )
    .await?;
    Ok(build_file_search_output(
        matches,
        hit_limit,
        extra_skips,
        Duration::default(),
    ))
}

/// Executes the `file_search` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    extra_skips: &[String],
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
        .filter(|path| !should_skip_search_path(Path::new(path), extra_skips))
        .collect::<Vec<_>>();
    let hit_limit = matches.len() > MAX_FILE_SEARCH_MATCHES;
    matches.truncate(MAX_FILE_SEARCH_MATCHES);

    Ok(build_file_search_output(
        matches,
        hit_limit,
        extra_skips,
        Duration::default(),
    ))
}

async fn collect_matches(
    root: &Path,
    current: &Path,
    matcher: &GlobMatcher,
    extra_skips: &[String],
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
            if should_skip_search_path(relative_path, extra_skips) {
                continue;
            }
            if Box::pin(collect_matches(root, &path, matcher, extra_skips, matches)).await? {
                return Ok(true);
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if should_skip_search_path(relative_path, extra_skips) {
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

fn should_skip_search_path(path: &Path, extra_skips: &[String]) -> bool {
    path.components().any(|component| match component {
        Component::Normal(segment) => {
            SKIPPED_SEARCH_DIRS
                .iter()
                .any(|ignored| segment == OsStr::new(ignored))
                || extra_skips
                    .iter()
                    .any(|ignored| segment == OsStr::new(ignored.as_str()))
        }
        _ => false,
    })
}

fn build_file_search_output(
    mut matches: Vec<String>,
    hit_limit: bool,
    extra_skips: &[String],
    duration: Duration,
) -> ToolOutput {
    matches.sort();
    let skipped_directories = skipped_directory_names(extra_skips);

    let structured_matches = matches
        .iter()
        .map(|path| serde_json::json!({ "path": path }))
        .collect::<Vec<_>>();
    let structured = serde_json::json!({
        "matches": structured_matches,
        "truncated": hit_limit,
        "skipped_directories": skipped_directories.clone(),
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
    }
}

#[derive(Debug, Deserialize)]
struct FileSearchInput {
    pattern: String,
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

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::path::Path;

    use serde_json::json;
    use tempfile::tempdir;
    use tokio::fs;

    use super::{
        default_skipped_dirs, execute, load_moaignore, should_ignore_search_io_error,
        should_skip_search_path, skipped_directory_names,
    };

    #[test]
    fn skips_python_venv_directory() {
        let path = Path::new(".venv/lib/python3.12/site-packages/requests/api.py");
        assert!(should_skip_search_path(path, &[]));
    }

    #[test]
    fn skips_pycache_directory() {
        let path = Path::new("server/core/__pycache__/views.cpython-312.pyc");
        assert!(should_skip_search_path(path, &[]));
    }

    #[test]
    fn skips_custom_moaignore_entry() {
        let path = Path::new("data/fixtures/large-dataset.json");
        let extra = vec!["data".to_string()];
        assert!(should_skip_search_path(path, &extra));
    }

    #[test]
    fn does_not_skip_normal_source_files() {
        let path = Path::new("server/core/views.py");
        assert!(!should_skip_search_path(path, &[]));
    }

    #[test]
    fn skips_gradle_directory() {
        let path = Path::new(".gradle/caches/modules-2/files-2.1/com.google/guava.jar");
        assert!(should_skip_search_path(path, &[]));
    }

    #[test]
    fn skips_vendor_directory() {
        let path = Path::new("vendor/github.com/pkg/errors/errors.go");
        assert!(should_skip_search_path(path, &[]));
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
    async fn load_moaignore_reads_directory_names_and_ignores_comments() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(".moaignore"),
            "# comment\n\n data \nfixtures\n",
        )
        .await
        .unwrap();

        let ignored = load_moaignore(dir.path()).await;
        assert_eq!(ignored, vec!["data".to_string(), "fixtures".to_string()]);
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

        let output = execute(
            dir.path(),
            &json!({ "pattern": "**/*.py" }).to_string(),
            &[],
        )
        .await
        .unwrap();
        let rendered = output.to_text();

        assert!(rendered.contains("server/core/views.py"));
        assert!(!rendered.contains(".venv/lib/ignored.py"));
    }

    #[tokio::test]
    async fn execute_respects_custom_skip_directories() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("data")).await.unwrap();
        fs::create_dir_all(dir.path().join("src")).await.unwrap();
        fs::write(dir.path().join("data/fixtures.json"), "{}")
            .await
            .unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn demo() {}")
            .await
            .unwrap();

        let output = execute(
            dir.path(),
            &json!({ "pattern": "**/*" }).to_string(),
            &["data".to_string()],
        )
        .await
        .unwrap();
        let rendered = output.to_text();

        assert!(rendered.contains("src/lib.rs"));
        assert!(!rendered.contains("data/fixtures.json"));
    }

    #[test]
    fn skipped_directory_names_appends_custom_entries_once() {
        let skipped = skipped_directory_names(&["data".to_string(), "target".to_string()]);
        assert!(skipped.contains(&"data".to_string()));
        assert_eq!(
            skipped
                .iter()
                .filter(|name| name.as_str() == "target")
                .count(),
            1
        );
    }
}
