//! `str_replace` tool implementation.

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::{
    display_container_relative_path, docker_file_read, docker_file_write,
    resolve_container_workspace_path,
};
use crate::tools::edit_output::build_text_edit_output;
use crate::tools::file_read::resolve_sandbox_path;

const MAX_CONTEXT_LINES: usize = 4;
const MAX_DISAMBIGUATION_MATCHES: usize = 5;

/// Planned `str_replace` mutation used by executors and approval previews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedStrReplace {
    /// Full file contents after the edit is applied.
    pub updated_content: String,
    /// Existing snippet shown in approval previews.
    pub preview_before: String,
    /// Proposed snippet shown in approval previews.
    pub preview_after: String,
}

/// Executes the `str_replace` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: StrReplaceInput = serde_json::from_str(input)?;
    let path = resolve_sandbox_path(sandbox_dir, &params.path)?;
    let existing_content = read_existing_text_file(&path).await?;
    let planned = plan_str_replace(
        input,
        existing_content.as_deref(),
        &display_path(sandbox_dir, &path),
        MAX_CONTEXT_LINES,
    )?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&path, &planned.updated_content).await?;

    Ok(build_text_edit_output(
        &display_path(sandbox_dir, &path),
        existing_content.as_deref().unwrap_or_default(),
        &planned.updated_content,
        Duration::default(),
    ))
}

/// Executes the `str_replace` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: StrReplaceInput = serde_json::from_str(input)?;
    let path = resolve_container_workspace_path(workspace_root, &params.path)?;
    let existing_content = match docker_file_read(container_id, &path, timeout, hard_cancel_token)
        .await
    {
        Ok(content) => Some(content),
        Err(MoaError::ToolError(message)) if message.contains("No such file or directory") => None,
        Err(error) => return Err(error),
    };
    let planned = plan_str_replace(
        input,
        existing_content.as_deref(),
        &display_container_relative_path(workspace_root, &path),
        MAX_CONTEXT_LINES,
    )?;
    docker_file_write(
        container_id,
        &path,
        &planned.updated_content,
        timeout,
        hard_cancel_token,
    )
    .await?;

    Ok(build_text_edit_output(
        &display_container_relative_path(workspace_root, &path),
        existing_content.as_deref().unwrap_or_default(),
        &planned.updated_content,
        Duration::default(),
    ))
}

/// Computes the file mutation and approval preview for a `str_replace` invocation.
pub(crate) fn plan_str_replace(
    input: &str,
    existing_content: Option<&str>,
    display_path: &str,
    context_lines: usize,
) -> Result<PlannedStrReplace> {
    let params: StrReplaceInput = serde_json::from_str(input)?;

    if params.insert_after_line.is_some() {
        return Err(MoaError::ToolError(format!(
            "str_replace failed: line-based insertion is not supported for {display_path}. Use file_write to create or rewrite a file, or use str_replace with a non-empty old_str that matches existing text exactly once."
        )));
    }

    if params.old_str.is_empty() {
        return Err(MoaError::ToolError(format!(
            "str_replace failed: old_str must be non-empty for {display_path}. Use file_write to create a new file, or use str_replace with a unique existing snippet."
        )));
    }

    let content = existing_content.ok_or_else(|| {
        MoaError::ToolError(format!(
            "str_replace failed: cannot read {display_path}: file not found"
        ))
    })?;
    let matches = match_positions(content, &params.old_str);

    match matches.len() {
        0 => Err(MoaError::ToolError(format!(
            "str_replace failed: old_str not found in {display_path}. Make sure the string matches exactly, including whitespace and indentation."
        ))),
        1 => Ok(plan_unique_replacement(
            &params,
            content,
            matches[0],
            context_lines,
        )),
        count => Err(MoaError::ToolError(build_ambiguous_match_error(
            display_path,
            content,
            &params.old_str,
            &matches,
            count,
            context_lines,
        ))),
    }
}

fn plan_unique_replacement(
    params: &StrReplaceInput,
    content: &str,
    match_start: usize,
    context_lines: usize,
) -> PlannedStrReplace {
    let before = &content[..match_start];
    let after = &content[match_start + params.old_str.len()..];
    let mut updated_content =
        String::with_capacity(before.len() + params.new_str.len() + after.len());
    updated_content.push_str(before);
    updated_content.push_str(&params.new_str);
    updated_content.push_str(after);

    let start_line = line_number_at_offset(content, match_start);
    let old_line_count = line_count(&params.old_str);
    let new_line_count = line_count(&params.new_str);
    let preview_start = start_line.saturating_sub(context_lines).max(1);
    let preview_end_before =
        replacement_end_line(start_line, old_line_count).saturating_add(context_lines);
    let preview_end_after = replacement_end_line(start_line, new_line_count)
        .max(start_line)
        .saturating_add(context_lines);

    let preview_before = preview_lines(content, preview_start, preview_end_before);
    let preview_after = preview_lines(&updated_content, preview_start, preview_end_after);

    PlannedStrReplace {
        updated_content,
        preview_before,
        preview_after,
    }
}

fn build_ambiguous_match_error(
    display_path: &str,
    content: &str,
    old_str: &str,
    matches: &[usize],
    match_count: usize,
    context_lines: usize,
) -> String {
    let mut hints = String::new();
    let old_line_count = line_count(old_str);

    for (index, position) in matches.iter().take(MAX_DISAMBIGUATION_MATCHES).enumerate() {
        let start_line = line_number_at_offset(content, *position);
        let preview_start = start_line.saturating_sub(context_lines).max(1);
        let preview_end =
            replacement_end_line(start_line, old_line_count).saturating_add(context_lines);
        hints.push_str(&format!(
            "  match {}: line {}\n{}\n",
            index + 1,
            start_line,
            preview_lines(content, preview_start, preview_end)
        ));
    }

    if match_count > MAX_DISAMBIGUATION_MATCHES {
        hints.push_str(&format!(
            "  ... and {} more matches\n",
            match_count - MAX_DISAMBIGUATION_MATCHES
        ));
    }

    format!(
        "str_replace failed: old_str found {match_count} times in {display_path}. Include more surrounding context to make the match unique.\n{hints}"
    )
}

fn match_positions(content: &str, needle: &str) -> Vec<usize> {
    content
        .match_indices(needle)
        .map(|(index, _)| index)
        .collect()
}

fn line_number_at_offset(content: &str, byte_offset: usize) -> usize {
    content[..byte_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn line_count(content: &str) -> usize {
    if content.is_empty() {
        0
    } else {
        content.lines().count().max(1)
    }
}

fn replacement_end_line(start_line: usize, line_count: usize) -> usize {
    if line_count == 0 {
        start_line
    } else {
        start_line + line_count - 1
    }
}

fn preview_lines(content: &str, start_line: usize, end_line: usize) -> String {
    if content.is_empty() || end_line < start_line {
        return String::new();
    }

    let lines = content.split_inclusive('\n').collect::<Vec<_>>();
    let start_index = start_line.saturating_sub(1).min(lines.len());
    let end_index = end_line.min(lines.len());
    lines[start_index..end_index].concat()
}

async fn read_existing_text_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn display_path(sandbox_dir: &Path, path: &Path) -> String {
    path.strip_prefix(sandbox_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[derive(Debug, Deserialize)]
struct StrReplaceInput {
    path: String,
    #[serde(default)]
    old_str: String,
    #[serde(default)]
    new_str: String,
    insert_after_line: Option<usize>,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn str_replace_single_match() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("test.py"), "def foo():\n    return 1\n")
            .await
            .expect("write");

        let output = execute(
            dir.path(),
            r#"{"path":"test.py","old_str":"return 1","new_str":"return 42"}"#,
        )
        .await
        .expect("str_replace");

        let content = fs::read_to_string(dir.path().join("test.py"))
            .await
            .expect("read");
        assert!(content.contains("return 42"));
        assert!(!content.contains("return 1"));
        let rendered = output.to_text();
        assert!(rendered.starts_with("--- a/test.py\n+++ b/test.py\n"));
        assert!(rendered.contains("-    return 1"));
        assert!(rendered.contains("+    return 42"));
        assert!(!rendered.contains("starting at line 2"));
    }

    #[tokio::test]
    async fn str_replace_no_match_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("test.py"), "def foo():\n    return 1\n")
            .await
            .expect("write");

        let result = execute(
            dir.path(),
            r#"{"path":"test.py","old_str":"return 999","new_str":"x"}"#,
        )
        .await;

        let error = result.expect_err("expected error");
        assert!(error.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn str_replace_multiple_matches_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("test.py"), "x = 1\nx = 2\nx = 3\n")
            .await
            .expect("write");

        let result = execute(
            dir.path(),
            r#"{"path":"test.py","old_str":"x = ","new_str":"y = "}"#,
        )
        .await;

        let error = result.expect_err("expected error");
        assert!(error.to_string().contains("3 times"));
        assert!(error.to_string().contains("line 1"));
        assert!(error.to_string().contains("line 2"));
        assert!(error.to_string().contains("line 3"));
    }

    #[tokio::test]
    async fn str_replace_deletion() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("test.py"), "line1\ndelete_me\nline3\n")
            .await
            .expect("write");

        execute(
            dir.path(),
            r#"{"path":"test.py","old_str":"delete_me\n","new_str":""}"#,
        )
        .await
        .expect("delete");

        let content = fs::read_to_string(dir.path().join("test.py"))
            .await
            .expect("read");
        assert!(!content.contains("delete_me"));
        assert!(content.contains("line1\nline3"));
    }

    #[tokio::test]
    async fn str_replace_no_op_returns_notice() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("test.py"), "line1\nline2\n")
            .await
            .expect("write");

        let rendered = execute(
            dir.path(),
            r#"{"path":"test.py","old_str":"line2","new_str":"line2"}"#,
        )
        .await
        .expect("no-op diff")
        .to_text();
        assert_eq!(rendered, "[no changes written: test.py]");
    }

    #[tokio::test]
    async fn str_replace_rejects_line_based_insertion() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("test.py"), "line1\nline3\n")
            .await
            .expect("write");

        let error = execute(
            dir.path(),
            r#"{"path":"test.py","old_str":"","new_str":"line2","insert_after_line":1}"#,
        )
        .await
        .expect_err("expected error");

        assert!(
            error
                .to_string()
                .contains("line-based insertion is not supported")
        );
    }

    #[tokio::test]
    async fn str_replace_rejects_creation_without_old_str() {
        let dir = tempdir().expect("tempdir");

        let error = execute(
            dir.path(),
            r#"{"path":"nested/new.py","old_str":"","new_str":"print('hi')\n"}"#,
        )
        .await
        .expect_err("expected error");

        assert!(error.to_string().contains("old_str must be non-empty"));
        assert!(!dir.path().join("nested/new.py").exists());
    }
}
