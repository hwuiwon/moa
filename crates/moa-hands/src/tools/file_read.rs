//! `file_read` tool implementation.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use moa_core::{error::MoaError, error::Result, types::tools::ToolOutput};
use serde::Deserialize;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
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
    let resolved = match resolve_existing_sandbox_path(sandbox_dir, &params.path).await {
        Ok(resolved) => resolved,
        Err(error) => {
            // A miss under a skill package is a known failure mode: the model
            // guesses `.moa/skills/<name>.md` and retries the identical read.
            // Enrich only the not-found skill miss with the canonical activation
            // path so the model self-corrects on the first miss; every other
            // error (traversal, permission, non-skill miss) keeps its plain form.
            if let Some(guidance) =
                skill_path_miss_guidance(sandbox_dir, &params.path, &error).await
            {
                return Err(MoaError::ToolError(guidance));
            }
            return Err(error);
        }
    };
    if is_ranged(&params) {
        return render_ranged_file_read(&resolved.path, &resolved.display_path, &params).await;
    }
    let content = fs::read_to_string(&resolved.path).await?;

    Ok(render_file_read_output(
        &content,
        &resolved.display_path,
        &params,
    ))
}

/// Renders a `file_read` response from content already loaded by a provider or trusted manifest.
pub fn execute_with_content(input: &str, display_path: &str, content: &str) -> Result<ToolOutput> {
    let params: FileReadInput = serde_json::from_str(input)?;
    Ok(render_file_read_output(content, display_path, &params))
}

/// Path fragments marking a `file_read` target as a skill-package read.
///
/// A miss on such a path is enriched with the canonical activation path; any
/// other miss keeps its plain not-found error.
const SKILL_PACKAGE_MARKERS: [&str; 2] = [".moa/", "skills/"];

/// Maximum materialized skill directories named in a single miss-guidance message.
const MAX_LISTED_SKILL_DIRS: usize = 20;

/// Returns true when a requested path targets a skill package.
fn references_skill_path(raw_path: &str) -> bool {
    let lowered = raw_path.to_ascii_lowercase();
    SKILL_PACKAGE_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Builds corrective guidance for a skill-package `file_read` miss.
///
/// Returns `None` unless the error is a not-found miss *and* the path references
/// a skill package, so unrelated reads keep their plain error. On a skill miss it
/// reshapes the guessed path to the canonical `.moa/skills/<slug>/SKILL.md` form
/// and, when the sandbox has a materialized `.moa/skills/` directory, names the
/// available skill directories (capped). The work is one bounded directory
/// listing on the miss path only; successful reads are untouched.
async fn skill_path_miss_guidance(
    sandbox_dir: &Path,
    raw_path: &str,
    error: &MoaError,
) -> Option<String> {
    let is_not_found = matches!(error, MoaError::Io(io) if io.kind() == ErrorKind::NotFound);
    if !is_not_found || !references_skill_path(raw_path) {
        return None;
    }

    let mut guidance = format!(
        "no file at `{raw_path}`. Skill packages materialize at \
         `.moa/skills/<slug>/SKILL.md`; read that exact path, not a bare \
         `.moa/skills/<name>.md`."
    );
    if let Some(canonical) = canonical_skill_path_suggestion(raw_path) {
        guidance.push_str(&format!(" Did you mean `{canonical}`?"));
    }
    let available = list_materialized_skill_slugs(sandbox_dir).await;
    if !available.is_empty() {
        guidance.push_str(&format!(" Available skills: {}.", available.join(", ")));
    }
    Some(guidance)
}

/// Reshapes a guessed skill path into the canonical `.moa/skills/<slug>/SKILL.md`.
///
/// Uses the final path segment's `.md` stem as the slug so a guess like
/// `.moa/skills/memory-privacy-check.md` (or `.well-known/skills/memory-privacy-check.md`)
/// maps to `.moa/skills/memory-privacy-check/SKILL.md`. Returns `None` when there
/// is no `.md` segment or it is already `SKILL.md`.
fn canonical_skill_path_suggestion(raw_path: &str) -> Option<String> {
    let last = Path::new(raw_path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => segment.to_str(),
            _ => None,
        })
        .next_back()?;
    let stem = last.strip_suffix(".md")?;
    if stem.is_empty() || stem.eq_ignore_ascii_case("skill") {
        return None;
    }
    Some(format!(".moa/skills/{stem}/SKILL.md"))
}

/// Lists the materialized skill directories under `<sandbox>/.moa/skills`.
///
/// Returns an empty vector when the directory is absent. The names are sorted
/// and capped so the guidance message is deterministic and bounded.
async fn list_materialized_skill_slugs(sandbox_dir: &Path) -> Vec<String> {
    let skills_root = sandbox_dir.join(".moa").join("skills");
    let Ok(mut entries) = fs::read_dir(&skills_root).await else {
        return Vec::new();
    };

    let mut slugs = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let is_dir = entry
            .file_type()
            .await
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false);
        if is_dir && let Some(name) = entry.file_name().to_str() {
            slugs.push(name.to_string());
        }
    }
    slugs.sort();
    slugs.truncate(MAX_LISTED_SKILL_DIRS);
    slugs
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
    if is_ranged(&params) {
        return render_ranged_file_read(&resolved.path, &display_path, &params).await;
    }
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

    if !is_ranged(params) {
        if content.len() <= MAX_UNSCOPED_FILE_READ_BYTES && total_lines <= LARGE_FILE_HINT_LINES {
            return ToolOutput::text(content.to_string(), Duration::default());
        }

        let end_line = total_lines.min(MAX_READ_RANGE_LINES);
        return ToolOutput::text(
            render_numbered_range(
                &lines[..end_line],
                display_path,
                total_lines,
                1,
                end_line,
                true,
            ),
            Duration::default(),
        );
    }

    let (start_line, end_line) = resolve_line_range(params, total_lines);
    ToolOutput::text(
        render_numbered_range(
            slice_for_range(&lines, start_line, end_line),
            display_path,
            total_lines,
            start_line,
            end_line,
            false,
        ),
        Duration::default(),
    )
}

/// Renders a ranged `file_read` by streaming the file and materializing only the
/// requested window, so a bounded range on a large file does not load the whole
/// file into memory. The full file is still streamed to report an accurate total
/// line count, but peak allocation is bounded to the window size.
async fn render_ranged_file_read(
    path: &Path,
    display_path: &str,
    params: &FileReadInput,
) -> Result<ToolOutput> {
    let collect_start = params.start_line.unwrap_or(1).max(1);
    let collect_end = params
        .end_line
        .map(|end| end.min(collect_start.saturating_add(MAX_READ_RANGE_LINES - 1)))
        .unwrap_or_else(|| collect_start.saturating_add(MAX_READ_RANGE_LINES - 1));

    let mut reader = BufReader::new(fs::File::open(path).await?);
    let mut window: Vec<String> = Vec::new();
    let mut total_lines = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        total_lines += 1;
        if total_lines >= collect_start && total_lines <= collect_end {
            window.push(line.clone());
        }
    }

    let (start_line, end_line) = resolve_line_range(params, total_lines);
    // The resolved start can only fall below the collected window when the file is
    // shorter than the requested start (a clamp to `total_lines`); that window is
    // empty, so fall back to a full read for those rare out-of-range requests.
    if start_line != 0 && start_line < collect_start {
        let content = fs::read_to_string(path).await?;
        return Ok(render_file_read_output(&content, display_path, params));
    }

    let window_refs: Vec<&str> = window.iter().map(String::as_str).collect();
    let range: &[&str] = if start_line == 0 || end_line == 0 || start_line > end_line {
        &[]
    } else {
        let from = start_line - collect_start;
        let to = (end_line - collect_start + 1).min(window_refs.len());
        &window_refs[from..to]
    };

    Ok(ToolOutput::text(
        render_numbered_range(
            range,
            display_path,
            total_lines,
            start_line,
            end_line,
            false,
        ),
        Duration::default(),
    ))
}

fn is_ranged(params: &FileReadInput) -> bool {
    params.start_line.is_some() || params.end_line.is_some()
}

/// Returns the `start_line..=end_line` slice of already-loaded lines, or empty
/// when the range is degenerate or out of bounds.
fn slice_for_range<'a>(lines: &'a [&'a str], start_line: usize, end_line: usize) -> &'a [&'a str] {
    if start_line == 0 || end_line == 0 || start_line > end_line || start_line > lines.len() {
        return &[];
    }
    let end = end_line.min(lines.len());
    &lines[start_line - 1..end]
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
    range_lines: &[&str],
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
    for (offset, line) in range_lines.iter().enumerate() {
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

    #[tokio::test]
    async fn skill_path_miss_returns_canonical_activation_guidance() {
        // Pins: a miss on the guessed `.moa/skills/<name>.md` (the live S085/S090
        // failure) is corrective — it names the exact `.moa/skills/<name>/SKILL.md`
        // form so the model reads the right path on the first miss instead of
        // retrying the identical guess.
        let dir = tempdir().expect("tempdir");

        let error = execute(
            dir.path(),
            r#"{"path":".moa/skills/memory-privacy-check.md"}"#,
        )
        .await
        .expect_err("missing skill file should error");

        let MoaError::ToolError(message) = error else {
            panic!("skill miss should surface corrective ToolError guidance: {error:?}");
        };
        assert!(
            message.contains(".moa/skills/memory-privacy-check/SKILL.md"),
            "guidance must name the canonical activation path: {message}"
        );
    }

    #[tokio::test]
    async fn well_known_skill_path_miss_is_also_corrective() {
        // Pins: the second observed guessed shape (`.well-known/skills/<name>.md`)
        // is reshaped to the same canonical `.moa/skills/<name>/SKILL.md` form.
        let dir = tempdir().expect("tempdir");

        let error = execute(
            dir.path(),
            r#"{"path":".well-known/skills/memory-privacy-check.md"}"#,
        )
        .await
        .expect_err("missing skill file should error");

        let MoaError::ToolError(message) = error else {
            panic!("skill miss should surface corrective ToolError guidance: {error:?}");
        };
        assert!(
            message.contains(".moa/skills/memory-privacy-check/SKILL.md"),
            "guidance must name the canonical activation path: {message}"
        );
    }

    #[tokio::test]
    async fn skill_path_miss_lists_materialized_skill_directories() {
        // Pins: when the sandbox has a materialized `.moa/skills/` directory, the
        // miss guidance names the actual available skill directories so the model
        // can pick the real slug, not only the reshaped guess.
        let dir = tempdir().expect("tempdir");
        for slug in ["memory-privacy-check", "refund-policy"] {
            fs::create_dir_all(dir.path().join(".moa").join("skills").join(slug))
                .await
                .expect("create skill dir");
        }

        let error = execute(dir.path(), r#"{"path":".moa/skills/privacy.md"}"#)
            .await
            .expect_err("missing skill file should error");

        let MoaError::ToolError(message) = error else {
            panic!("skill miss should surface corrective ToolError guidance: {error:?}");
        };
        assert!(
            message.contains("Available skills: memory-privacy-check, refund-policy"),
            "guidance must list materialized skill directories: {message}"
        );
    }

    #[tokio::test]
    async fn unrelated_path_miss_keeps_plain_not_found_error() {
        // Pins: a miss on a non-skill path is NOT decorated with skill guidance and
        // keeps its plain I/O not-found error.
        let dir = tempdir().expect("tempdir");

        let error = execute(dir.path(), r#"{"path":"docs/nope.txt"}"#)
            .await
            .expect_err("missing file should error");

        assert!(
            matches!(&error, MoaError::Io(io) if io.kind() == ErrorKind::NotFound),
            "unrelated miss should keep the plain not-found error: {error:?}"
        );
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
