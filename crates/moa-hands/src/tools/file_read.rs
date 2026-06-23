//! `file_read` tool implementation.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use moa_core::{MoaError, Result, ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use crate::tools::docker_file::{
    display_container_relative_path, docker_file_read, resolve_container_workspace_path,
};

const MAX_UNSCOPED_FILE_READ_BYTES: usize = 32 * 1024;
const MAX_READ_RANGE_LINES: usize = 200;
const LARGE_FILE_HINT_LINES: usize = 200;

/// Executes the `file_read` tool against a sandbox directory.
pub async fn execute(sandbox_dir: &Path, input: &str) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    let resolved = resolve_existing_sandbox_path(sandbox_dir, &params.path).await?;
    let content = fs::read_to_string(&resolved.path).await?;

    Ok(render_file_read_output(
        &content,
        &resolved.display_path,
        &params,
    ))
}

/// Executes the `file_read` tool inside an existing Docker sandbox.
pub async fn execute_docker(
    container_id: &str,
    workspace_root: &str,
    input: &str,
    timeout: Duration,
    hard_cancel_token: Option<&CancellationToken>,
) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    let path = resolve_container_workspace_path(workspace_root, &params.path)?;
    let content = docker_file_read(container_id, &path, timeout, hard_cancel_token).await?;
    let display_path = display_container_relative_path(workspace_root, &path);
    Ok(render_file_read_output(&content, &display_path, &params))
}

/// Executes the `file_read` tool against a Docker sandbox bind mount.
pub(crate) async fn execute_docker_bind_mount(
    host_workspace_root: &Path,
    workspace_root: &str,
    input: &str,
) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    let container_path = resolve_container_workspace_path(workspace_root, &params.path)?;
    let display_path = display_container_relative_path(workspace_root, &container_path);
    let resolved = resolve_existing_sandbox_path(host_workspace_root, &display_path).await?;
    let content = fs::read_to_string(&resolved.path).await?;
    Ok(render_file_read_output(&content, &display_path, &params))
}

/// A sandbox path resolved to a canonical host path and stable display path.
pub(crate) struct ResolvedSandboxPath {
    /// Canonical host path that passed sandbox containment checks.
    pub(crate) path: PathBuf,
    /// Workspace-relative path to show in tool output.
    pub(crate) display_path: String,
}

/// Resolves a user-provided relative path inside a sandbox root.
pub fn resolve_sandbox_path(sandbox_dir: &Path, raw_path: &str) -> Result<PathBuf> {
    let logical_path = Path::new(raw_path);
    if logical_path.is_absolute() {
        return Err(MoaError::PermissionDenied(format!(
            "path must stay within the sandbox: {raw_path}"
        )));
    }

    for component in logical_path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(MoaError::PermissionDenied(format!(
                "path traversal is not allowed: {raw_path}"
            )));
        }
    }

    Ok(sandbox_dir.join(logical_path))
}

/// Resolves an existing sandbox file and rejects symlink escapes.
pub(crate) async fn resolve_existing_sandbox_path(
    sandbox_dir: &Path,
    raw_path: &str,
) -> Result<ResolvedSandboxPath> {
    let candidate = resolve_sandbox_path(sandbox_dir, raw_path)?;
    let display_path = sandbox_display_path(raw_path);
    let root = fs::canonicalize(sandbox_dir).await?;
    let canonical = fs::canonicalize(&candidate).await?;
    ensure_inside_sandbox(&root, &canonical, raw_path)?;
    Ok(ResolvedSandboxPath {
        path: canonical,
        display_path,
    })
}

/// Resolves a writable sandbox file and rejects symlink escapes.
pub(crate) async fn resolve_writable_sandbox_path(
    sandbox_dir: &Path,
    raw_path: &str,
) -> Result<ResolvedSandboxPath> {
    let candidate = resolve_sandbox_path(sandbox_dir, raw_path)?;
    let display_path = sandbox_display_path(raw_path);
    let root = fs::canonicalize(sandbox_dir).await?;

    match fs::canonicalize(&candidate).await {
        Ok(canonical) => {
            ensure_inside_sandbox(&root, &canonical, raw_path)?;
            return Ok(ResolvedSandboxPath {
                path: canonical,
                display_path,
            });
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            reject_existing_symlink(&candidate, raw_path).await?;
        }
        Err(error) => return Err(error.into()),
    }

    let parent = candidate.parent().ok_or_else(|| {
        MoaError::PermissionDenied(format!("path must stay within the sandbox: {raw_path}"))
    })?;
    let _ = canonicalize_existing_ancestor(parent, &root, raw_path).await?;
    fs::create_dir_all(parent).await?;
    let canonical_parent = fs::canonicalize(parent).await?;
    ensure_inside_sandbox(&root, &canonical_parent, raw_path)?;
    let file_name = candidate.file_name().ok_or_else(|| {
        MoaError::PermissionDenied(format!(
            "path must identify a file inside the sandbox: {raw_path}"
        ))
    })?;

    Ok(ResolvedSandboxPath {
        path: canonical_parent.join(file_name),
        display_path,
    })
}

async fn reject_existing_symlink(path: &Path, raw_path: &str) -> Result<()> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(MoaError::PermissionDenied(
            format!("symlink escapes are not allowed in sandbox paths: {raw_path}"),
        )),
        Ok(_) => Err(MoaError::PermissionDenied(format!(
            "path could not be resolved inside the sandbox: {raw_path}"
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn canonicalize_existing_ancestor(
    path: &Path,
    sandbox_root: &Path,
    raw_path: &str,
) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        match fs::canonicalize(&current).await {
            Ok(canonical) => {
                ensure_inside_sandbox(sandbox_root, &canonical, raw_path)?;
                return Ok(canonical);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                reject_existing_symlink(&current, raw_path).await?;
                if !current.pop() {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn ensure_inside_sandbox(sandbox_root: &Path, path: &Path, raw_path: &str) -> Result<()> {
    if path.starts_with(sandbox_root) {
        return Ok(());
    }
    Err(MoaError::PermissionDenied(format!(
        "path resolves outside the sandbox: {raw_path}"
    )))
}

fn sandbox_display_path(raw_path: &str) -> String {
    let mut display = PathBuf::new();
    for component in Path::new(raw_path).components() {
        if let Component::Normal(segment) = component {
            display.push(segment);
        }
    }
    let display = display.display().to_string();
    if display.is_empty() {
        ".".to_string()
    } else {
        display
    }
}

fn render_file_read_output(
    content: &str,
    display_path: &str,
    params: &FileReadInput,
) -> ToolOutput {
    let lines = split_lines(content);
    let total_lines = lines.len();

    if params.start_line.is_none() && params.end_line.is_none() {
        if content.len() <= MAX_UNSCOPED_FILE_READ_BYTES && total_lines <= LARGE_FILE_HINT_LINES {
            return ToolOutput::text(content.to_string(), Duration::default());
        }

        return ToolOutput::text(
            render_numbered_range(
                &lines,
                display_path,
                total_lines,
                1,
                total_lines.min(MAX_READ_RANGE_LINES),
                true,
            ),
            Duration::default(),
        );
    }

    let (start_line, end_line) = resolve_line_range(params, total_lines);
    ToolOutput::text(
        render_numbered_range(
            &lines,
            display_path,
            total_lines,
            start_line,
            end_line,
            false,
        ),
        Duration::default(),
    )
}

fn resolve_line_range(params: &FileReadInput, total_lines: usize) -> (usize, usize) {
    if total_lines == 0 {
        return (0, 0);
    }

    let requested_start = params.start_line.unwrap_or(1).max(1);
    let start_line = requested_start.min(total_lines);
    let requested_end = params.end_line.unwrap_or(total_lines).max(start_line);
    let capped_end = requested_end.min(total_lines);
    let end_line = capped_end.min(start_line.saturating_add(MAX_READ_RANGE_LINES - 1));

    (start_line, end_line)
}

fn split_lines(content: &str) -> Vec<&str> {
    content.split_inclusive('\n').collect()
}

fn render_numbered_range(
    lines: &[&str],
    display_path: &str,
    total_lines: usize,
    start_line: usize,
    end_line: usize,
    truncated_unscoped: bool,
) -> String {
    let mut output = format!(
        "[showing lines {}-{} of {} total in {}]\n",
        start_line, end_line, total_lines, display_path
    );

    if total_lines == 0 || start_line == 0 || end_line == 0 || start_line > end_line {
        return output;
    }

    let width = end_line.to_string().len().max(2);
    for (offset, line) in lines[start_line - 1..end_line].iter().enumerate() {
        output.push_str(&format!(
            "{:>width$}\t{}",
            start_line + offset,
            line,
            width = width
        ));
        if !line.ends_with('\n') {
            output.push('\n');
        }
    }

    let requested_line_count = end_line - start_line + 1;
    if truncated_unscoped || requested_line_count >= MAX_READ_RANGE_LINES && end_line < total_lines
    {
        output.push_str(&format!(
            "\n[output truncated to {} lines; use a narrower range]\n",
            MAX_READ_RANGE_LINES
        ));
    }

    output
}

#[derive(Debug, Deserialize)]
struct FileReadInput {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;

    #[tokio::test]
    async fn file_read_returns_full_small_file_without_range() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("notes.txt"), "alpha\nbeta\n")
            .await
            .expect("write file");

        let output = execute(dir.path(), r#"{"path":"notes.txt"}"#)
            .await
            .expect("file read");

        assert_eq!(output.to_text(), "alpha\nbeta");
    }

    #[tokio::test]
    async fn file_read_reads_requested_line_range() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("notes.txt"),
            (1..=100)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .await
        .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"path":"notes.txt","start_line":10,"end_line":15}"#,
        )
        .await
        .expect("file read");

        let text = output.to_text();
        assert!(text.contains("[showing lines 10-15 of 100 total in notes.txt]"));
        assert!(text.contains("10\tline 10"));
        assert!(text.contains("15\tline 15"));
        assert!(!text.contains("9\tline 9"));
        assert!(!text.contains("16\tline 16"));
    }

    #[tokio::test]
    async fn file_read_clamps_out_of_range_values() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("test.txt"),
            (1..=10)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .await
        .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"path":"test.txt","start_line":8,"end_line":999}"#,
        )
        .await
        .expect("file read");

        let text = output.to_text();
        assert!(text.contains("[showing lines 8-10 of 10 total in test.txt]"));
        assert!(text.contains("8\tline 8"));
        assert!(text.contains("10\tline 10"));
    }

    #[tokio::test]
    async fn file_read_truncates_large_range_requests() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("big.txt"),
            (1..=1000)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .await
        .expect("write file");

        let output = execute(
            dir.path(),
            r#"{"path":"big.txt","start_line":1,"end_line":1000}"#,
        )
        .await
        .expect("file read");

        let text = output.to_text();
        assert!(text.contains("[showing lines 1-200 of 1000 total in big.txt]"));
        assert!(text.contains("[output truncated to 200 lines; use a narrower range]"));
        assert!(!text.contains("201\tline 201"));
    }

    #[tokio::test]
    async fn file_read_truncates_large_unscoped_reads_to_the_first_chunk() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("large.txt"),
            (1..=800)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .await
        .expect("write file");

        let output = execute(dir.path(), r#"{"path":"large.txt"}"#)
            .await
            .expect("file read");

        let text = output.to_text();
        assert!(text.contains("[showing lines 1-200 of 800 total in large.txt]"));
        assert!(text.contains("[output truncated to 200 lines; use a narrower range]"));
    }

    #[tokio::test]
    async fn file_read_supports_end_line_without_start_line() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("small.txt"),
            (1..=5)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .await
        .expect("write file");

        let output = execute(dir.path(), r#"{"path":"small.txt","end_line":3}"#)
            .await
            .expect("file read");

        let text = output.to_text();
        assert!(text.contains("[showing lines 1-3 of 5 total in small.txt]"));
        assert!(text.contains("1\tline 1"));
        assert!(text.contains("3\tline 3"));
        assert!(!text.contains("4\tline 4"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_read_rejects_symlink_escape() {
        // Pins: host-local file reads must not follow sandbox symlinks to outside files.
        let dir = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside tempdir");
        fs::write(outside.path().join("secret.txt"), "do not read")
            .await
            .expect("write outside file");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .expect("create symlink");

        let error = execute(dir.path(), r#"{"path":"link.txt"}"#)
            .await
            .expect_err("symlink escape should be rejected");

        assert!(matches!(error, MoaError::PermissionDenied(_)));
    }
}
