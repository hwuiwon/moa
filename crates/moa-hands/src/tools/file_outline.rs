//! `file_outline` tool implementation.

use std::path::Path;
use std::time::Duration;

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::{
    display_container_relative_path, docker_file_read, resolve_container_workspace_path,
};
use crate::tools::file_read::resolve_sandbox_path;

const MAX_OUTLINE_ENTRIES: usize = 256;

/// Executes the `file_outline` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileOutlineInput = serde_json::from_str(input)?;
    let path = resolve_sandbox_path(sandbox_dir, &params.path)?;
    let content = fs::read_to_string(&path).await?;
    let display_path = path.strip_prefix(sandbox_dir).unwrap_or(&path).display();

    render_outline(&content, &display_path.to_string(), &params)
}

/// Executes the `file_outline` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: FileOutlineInput = serde_json::from_str(input)?;
    let path = resolve_container_workspace_path(workspace_root, &params.path)?;
    let content = docker_file_read(container_id, &path, timeout, hard_cancel_token).await?;
    let display_path = display_container_relative_path(workspace_root, &path);

    render_outline(&content, &display_path, &params)
}

fn render_outline(
    content: &str,
    display_path: &str,
    params: &FileOutlineInput,
) -> Result<ToolOutput> {
    if !params.path.ends_with(".py") {
        return Err(MoaError::ToolError(format!(
            "file_outline currently supports Python files only: {}",
            params.path
        )));
    }

    let outline = build_python_outline(content);
    let matching = filter_outline(&outline, params.symbol.as_deref());
    if matching.is_empty() {
        let target = params.symbol.as_deref().unwrap_or("top-level symbols");
        return Err(MoaError::ToolError(format!(
            "file_outline could not find {target} in {}",
            params.path
        )));
    }

    let mut output = format!("[outline for {display_path}]\n");
    for entry in matching.iter().take(MAX_OUTLINE_ENTRIES) {
        let indent = if entry.kind == OutlineKind::Method {
            "  "
        } else {
            ""
        };
        output.push_str(&format!(
            "{indent}{} {} {}\n",
            entry.line,
            entry.kind.label(),
            entry.name
        ));
    }
    if matching.len() > MAX_OUTLINE_ENTRIES {
        output.push_str(&format!(
            "\n[outline truncated to {} entries; narrow the symbol]\n",
            MAX_OUTLINE_ENTRIES
        ));
    }

    Ok(ToolOutput::text(output, Duration::default()))
}

fn build_python_outline(content: &str) -> Vec<OutlineEntry> {
    let mut outline = Vec::new();
    let mut current_class: Option<(String, usize)> = None;
    let mut multiline_string_delimiter: Option<&'static str> = None;

    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(delimiter) = multiline_string_delimiter {
            if triple_quote_count(trimmed, delimiter) % 2 == 1 {
                multiline_string_delimiter = None;
            }
            continue;
        }

        let indent = line.len() - trimmed.len();
        if indent == 0 {
            current_class = None;

            if let Some(name) = parse_python_class_name(trimmed) {
                outline.push(OutlineEntry {
                    line: line_number,
                    kind: OutlineKind::Class,
                    name: name.clone(),
                    parent: None,
                });
                current_class = Some((name, indent));
                continue;
            }

            if let Some(name) = parse_python_function_name(trimmed) {
                outline.push(OutlineEntry {
                    line: line_number,
                    kind: OutlineKind::Function,
                    name,
                    parent: None,
                });
            }
            continue;
        }

        if let Some((class_name, class_indent)) = &current_class
            && indent > *class_indent
            && let Some(name) = parse_python_function_name(trimmed)
        {
            outline.push(OutlineEntry {
                line: line_number,
                kind: OutlineKind::Method,
                name,
                parent: Some(class_name.clone()),
            });
        }

        if let Some(delimiter) = opening_multiline_string_delimiter(trimmed) {
            multiline_string_delimiter = Some(delimiter);
        }
    }

    outline
}

fn filter_outline<'a>(outline: &'a [OutlineEntry], symbol: Option<&str>) -> Vec<&'a OutlineEntry> {
    let Some(symbol) = symbol else {
        return outline.iter().collect();
    };

    let mut matching = outline
        .iter()
        .filter(|entry| entry.name == symbol || entry.parent.as_deref() == Some(symbol))
        .collect::<Vec<_>>();

    if matching
        .iter()
        .all(|entry| entry.kind != OutlineKind::Class)
        && let Some(parent_name) = matching.iter().find_map(|entry| entry.parent.as_deref())
        && let Some(parent) = outline
            .iter()
            .find(|entry| entry.kind == OutlineKind::Class && entry.name == parent_name)
    {
        matching.insert(0, parent);
    }

    matching
}

fn parse_python_class_name(trimmed: &str) -> Option<String> {
    let suffix = trimmed.strip_prefix("class ")?;
    let name = suffix.split(['(', ':']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn parse_python_function_name(trimmed: &str) -> Option<String> {
    let suffix = trimmed
        .strip_prefix("def ")
        .or_else(|| trimmed.strip_prefix("async def "))?;
    let name = suffix.split('(').next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn opening_multiline_string_delimiter(line: &str) -> Option<&'static str> {
    ["\"\"\"", "'''"]
        .into_iter()
        .find(|delimiter| triple_quote_count(line, delimiter) % 2 == 1)
}

fn triple_quote_count(line: &str, delimiter: &str) -> usize {
    line.match_indices(delimiter).count()
}

#[derive(Debug, Deserialize)]
struct FileOutlineInput {
    path: String,
    symbol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OutlineEntry {
    line: usize,
    kind: OutlineKind,
    name: String,
    parent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutlineKind {
    Class,
    Function,
    Method,
}

impl OutlineKind {
    fn label(self) -> &'static str {
        match self {
            Self::Class => "class",
            Self::Function => "function",
            Self::Method => "method",
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;

    #[tokio::test]
    async fn file_outline_lists_python_class_methods() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("demo.py"),
            "class CallViewSet:\n    def start(self):\n        pass\n\n    async def outbound(self):\n        pass\n",
        )
        .await
        .expect("write file");

        let output = execute(dir.path(), r#"{"path":"demo.py","symbol":"CallViewSet"}"#)
            .await
            .expect("outline");
        let text = output.to_text();

        assert!(text.contains("1 class CallViewSet"));
        assert!(text.contains("  2 method start"));
        assert!(text.contains("  5 method outbound"));
    }

    #[tokio::test]
    async fn file_outline_can_focus_on_a_single_method() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("demo.py"),
            "class CallViewSet:\n    def start(self):\n        pass\n\n    def outbound(self):\n        pass\n",
        )
        .await
        .expect("write file");

        let output = execute(dir.path(), r#"{"path":"demo.py","symbol":"outbound"}"#)
            .await
            .expect("outline");
        let text = output.to_text();

        assert!(text.contains("1 class CallViewSet"));
        assert!(text.contains("  5 method outbound"));
        assert!(!text.contains("  2 method start"));
    }

    #[tokio::test]
    async fn file_outline_keeps_class_context_across_multiline_docstrings() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("demo.py"),
            concat!(
                "class CallViewSet:\n",
                "    \"\"\"\n",
                "TL;DR\n",
                "Caller dials in.\n",
                "    \"\"\"\n",
                "    def start(self):\n",
                "        pass\n",
            ),
        )
        .await
        .expect("write file");

        let output = execute(dir.path(), r#"{"path":"demo.py","symbol":"CallViewSet"}"#)
            .await
            .expect("outline");
        let text = output.to_text();

        assert!(text.contains("1 class CallViewSet"));
        assert!(text.contains("  6 method start"));
    }

    #[tokio::test]
    async fn file_outline_errors_when_symbol_is_missing() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("demo.py"),
            "def hello():\n    return 'world'\n",
        )
        .await
        .expect("write file");

        let error = execute(dir.path(), r#"{"path":"demo.py","symbol":"CallViewSet"}"#)
            .await
            .expect_err("missing symbol should fail");

        assert!(error.to_string().contains("could not find CallViewSet"));
    }

    #[tokio::test]
    async fn file_outline_rejects_non_python_files() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("demo.rs"), "fn main() {}\n")
            .await
            .expect("write file");

        let error = execute(dir.path(), r#"{"path":"demo.rs"}"#)
            .await
            .expect_err("non-python should fail");

        assert!(error.to_string().contains("supports Python files only"));
    }
}
