//! `grep` tool implementation.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use moa_core::{MoaError, Result, ToolContent, ToolOutput};
use regex::Regex;
use serde::Deserialize;

use crate::tools::file_read::resolve_sandbox_path;
use crate::tools::file_search::should_skip_search_path_static;

const MAX_MATCHES: usize = 100;
const MAX_CONTEXT_LINES: usize = 5;
const MAX_LINE_LENGTH: usize = 500;

/// Executes the `grep` tool against a sandbox directory.
pub async fn execute(
    sandbox_dir: &Path,
    input: &str,
    extra_skips: &[String],
) -> Result<ToolOutput> {
    let params: GrepInput = serde_json::from_str(input)?;
    let search_root = resolve_search_root(sandbox_dir, params.path.as_deref())?;
    let pattern = if params.literal.unwrap_or(false) {
        regex::escape(&params.pattern)
    } else {
        params.pattern
    };
    let regex =
        Regex::new(&pattern).map_err(|error| MoaError::ValidationError(error.to_string()))?;
    let context_lines = params.context_lines.unwrap_or(0).min(MAX_CONTEXT_LINES);

    let sandbox_dir = sandbox_dir.to_path_buf();
    let search_root = search_root.to_path_buf();
    let extra_skips = extra_skips.to_vec();
    let started = Instant::now();
    let outcome = tokio::task::spawn_blocking(move || {
        search_workspace(
            &sandbox_dir,
            &search_root,
            &regex,
            context_lines,
            &extra_skips,
        )
    })
    .await
    .map_err(|error| MoaError::ToolError(format!("grep search task failed: {error}")))??;

    Ok(build_grep_output(outcome, started.elapsed()))
}

fn resolve_search_root(sandbox_dir: &Path, relative_path: Option<&str>) -> Result<PathBuf> {
    match relative_path {
        Some(path) => resolve_sandbox_path(sandbox_dir, path),
        None => Ok(sandbox_dir.to_path_buf()),
    }
}

fn search_workspace(
    sandbox_dir: &Path,
    search_root: &Path,
    regex: &Regex,
    context_lines: usize,
    extra_skips: &[String],
) -> Result<SearchOutcome> {
    let mut matches = Vec::new();
    let mut files_searched = 0usize;
    let mut truncated = false;
    let walker = WalkBuilder::new(search_root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .parents(true)
        .follow_links(false)
        .build();

    for entry in walker {
        if matches.len() >= MAX_MATCHES {
            truncated = true;
            break;
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        let relative_path = path.strip_prefix(sandbox_dir).unwrap_or(path);
        if should_skip_search_path_static(relative_path, extra_skips) {
            continue;
        }

        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        files_searched += 1;

        let lines = content.lines().collect::<Vec<_>>();
        for (line_index, line) in lines.iter().enumerate() {
            if matches.len() >= MAX_MATCHES {
                truncated = true;
                break;
            }
            if !regex.is_match(line) {
                continue;
            }

            matches.push(GrepMatch {
                path: relative_path.display().to_string(),
                line: line_index + 1,
                text: truncate_line(line),
                context: collect_context(&lines, line_index, context_lines),
            });
        }
    }

    Ok(SearchOutcome {
        matches,
        files_searched,
        truncated,
    })
}

fn collect_context(lines: &[&str], line_index: usize, context_lines: usize) -> Vec<ContextLine> {
    if context_lines == 0 {
        return Vec::new();
    }

    let start = line_index.saturating_sub(context_lines);
    let end = (line_index + context_lines + 1).min(lines.len());
    (start..end)
        .map(|context_index| ContextLine {
            line: context_index + 1,
            text: truncate_line(lines[context_index]),
            is_match: context_index == line_index,
        })
        .collect()
}

fn truncate_line(line: &str) -> String {
    let char_count = line.chars().count();
    if char_count <= MAX_LINE_LENGTH {
        return line.to_string();
    }

    let prefix = line.chars().take(MAX_LINE_LENGTH).collect::<String>();
    format!("{prefix}...")
}

fn build_grep_output(outcome: SearchOutcome, duration: Duration) -> ToolOutput {
    let mut summary = if outcome.matches.is_empty() {
        "No matching lines found.".to_string()
    } else {
        outcome
            .matches
            .iter()
            .map(render_match)
            .collect::<Vec<_>>()
            .join("\n")
    };

    if outcome.truncated {
        summary.push_str(&format!(
            "\n\n[search truncated at {} matches; narrow the pattern or search a subdirectory]",
            MAX_MATCHES
        ));
    }
    summary.push_str(&format!(
        "\n\n[{} files searched in {:?}]",
        outcome.files_searched, duration
    ));

    let structured = serde_json::json!({
        "match_count": outcome.matches.len(),
        "truncated": outcome.truncated,
        "files_searched": outcome.files_searched,
        "matches": outcome.matches.iter().map(|entry| serde_json::json!({
            "path": entry.path,
            "line": entry.line,
            "text": entry.text,
            "context": entry.context.iter().map(|context| serde_json::json!({
                "line": context.line,
                "text": context.text,
                "is_match": context.is_match,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });

    ToolOutput {
        content: vec![ToolContent::Text { text: summary }],
        is_error: false,
        structured: Some(structured),
        duration,
        truncated: false,
    }
}

fn render_match(entry: &GrepMatch) -> String {
    if entry.context.is_empty() {
        return format!("{}:{}:{}", entry.path, entry.line, entry.text);
    }

    let width = entry
        .context
        .last()
        .map(|context| context.line.to_string().len())
        .unwrap_or(2)
        .max(2);
    let mut block = format!("{}:{}\n", entry.path, entry.line);
    for context in &entry.context {
        let marker = if context.is_match { ">" } else { " " };
        block.push_str(&format!(
            "{} {:>width$} | {}\n",
            marker,
            context.line,
            context.text,
            width = width
        ));
    }
    block.trim_end().to_string()
}

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    context_lines: Option<usize>,
    literal: Option<bool>,
}

#[derive(Debug)]
struct SearchOutcome {
    matches: Vec<GrepMatch>,
    files_searched: usize,
    truncated: bool,
}

#[derive(Debug)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
    context: Vec<ContextLine>,
}

#[derive(Debug)]
struct ContextLine {
    line: usize,
    text: String,
    is_match: bool,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;

    #[tokio::test]
    async fn grep_finds_matching_lines() {
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

        let output = execute(dir.path(), r#"{"pattern":"class.*ViewSet"}"#, &[])
            .await
            .expect("grep");

        assert!(
            output
                .to_text()
                .contains("server/core/views.py:1:class CallViewSet:")
        );
    }

    #[tokio::test]
    async fn grep_supports_literal_matching() {
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
            r#"{"pattern":"router.register(r\"calls\", views.CallViewSet, basename=\"calls\")","literal":true}"#,
            &[],
        )
        .await
        .expect("grep");

        assert!(output.to_text().contains("basename=\"calls\""));
    }

    #[tokio::test]
    async fn grep_includes_context_lines() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("demo.py"),
            "line 1\nline 2\nCallViewSet\nline 4\nline 5\n",
        )
        .await
        .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"pattern":"CallViewSet","context_lines":2}"#,
            &[],
        )
        .await
        .expect("grep");
        let text = output.to_text();

        assert!(text.contains("demo.py:3"));
        assert!(text.contains("  1 | line 1"));
        assert!(text.contains(">  3 | CallViewSet"));
        assert!(text.contains("  5 | line 5"));
    }

    #[tokio::test]
    async fn grep_respects_gitignore() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join(".gitignore"), "ignored.py\n")
            .await
            .expect("write gitignore");
        fs::write(dir.path().join("ignored.py"), "CallViewSet\n")
            .await
            .expect("write ignored file");
        fs::write(dir.path().join("views.py"), "CallViewSet\n")
            .await
            .expect("write visible file");

        let output = execute(dir.path(), r#"{"pattern":"CallViewSet"}"#, &[])
            .await
            .expect("grep");
        let text = output.to_text();

        assert!(text.contains("views.py:1:CallViewSet"));
        assert!(!text.lines().any(|line| line.starts_with("ignored.py:1:")));
    }

    #[tokio::test]
    async fn grep_respects_skip_directories() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".venv/lib"))
            .await
            .expect("create dirs");
        fs::create_dir_all(dir.path().join("server/core"))
            .await
            .expect("create dirs");
        fs::write(dir.path().join(".venv/lib/ignored.py"), "CallViewSet\n")
            .await
            .expect("write ignored file");
        fs::write(dir.path().join("server/core/views.py"), "CallViewSet\n")
            .await
            .expect("write visible file");

        let output = execute(dir.path(), r#"{"pattern":"CallViewSet"}"#, &[])
            .await
            .expect("grep");
        let text = output.to_text();

        assert!(text.contains("server/core/views.py:1:CallViewSet"));
        assert!(!text.contains(".venv/lib/ignored.py"));
    }

    #[tokio::test]
    async fn grep_truncates_after_max_matches() {
        let dir = tempdir().expect("tempdir");
        let content = (1..=150)
            .map(|index| format!("CallViewSet {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.path().join("views.py"), content)
            .await
            .expect("write file");

        let output = execute(dir.path(), r#"{"pattern":"CallViewSet"}"#, &[])
            .await
            .expect("grep");

        assert!(
            output
                .to_text()
                .contains("[search truncated at 100 matches")
        );
    }

    #[tokio::test]
    async fn grep_scopes_to_subdirectory() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("server/core"))
            .await
            .expect("create dirs");
        fs::create_dir_all(dir.path().join("tests"))
            .await
            .expect("create dirs");
        fs::write(dir.path().join("server/core/views.py"), "CallViewSet\n")
            .await
            .expect("write file");
        fs::write(dir.path().join("tests/test_views.py"), "CallViewSet\n")
            .await
            .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"pattern":"CallViewSet","path":"server"}"#,
            &[],
        )
        .await
        .expect("grep");
        let text = output.to_text();

        assert!(text.contains("server/core/views.py:1:CallViewSet"));
        assert!(!text.contains("tests/test_views.py"));
    }

    #[tokio::test]
    async fn grep_skips_binary_files() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("binary.bin"), b"CallViewSet\0binary")
            .await
            .expect("write binary file");

        let output = execute(dir.path(), r#"{"pattern":"CallViewSet"}"#, &[])
            .await
            .expect("grep");

        assert!(!output.to_text().contains("binary.bin"));
    }
}
