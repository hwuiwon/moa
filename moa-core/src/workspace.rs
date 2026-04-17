//! Workspace-level discovery helpers.

use std::path::Path;

const INSTRUCTION_FILE_NAME: &str = "AGENTS.md";
const MAX_INSTRUCTION_FILE_BYTES: usize = 32_768;

/// Discovers and loads a workspace instruction file from the given root.
///
/// The `AGENTS.md` file is the only supported source. Oversized files are truncated at a
/// line boundary up to 32 KiB to keep prompt injection concise and predictable.
///
/// NOTE: caller must wrap in `tokio::task::spawn_blocking` when calling from an async context.
/// Use [`discover_workspace_instructions_async`] for a fully async alternative.
pub fn discover_workspace_instructions(workspace_root: &Path) -> Option<String> {
    let path = workspace_root.join(INSTRUCTION_FILE_NAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return None,
    };

    if bytes.len() > MAX_INSTRUCTION_FILE_BYTES {
        tracing::warn!(
            path = %path.display(),
            size = bytes.len(),
            max = MAX_INSTRUCTION_FILE_BYTES,
            "workspace instruction file exceeds size limit, truncating"
        );
        let truncated = &bytes[..MAX_INSTRUCTION_FILE_BYTES];
        let end = truncated
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(MAX_INSTRUCTION_FILE_BYTES);
        return Some(String::from_utf8_lossy(&bytes[..end]).into_owned());
    }

    tracing::info!(
        path = %path.display(),
        size = bytes.len(),
        "loaded workspace instruction file"
    );
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Async variant of [`discover_workspace_instructions`] that uses `tokio::fs`.
///
/// Prefer this over the sync version when calling from an `async` context.
pub async fn discover_workspace_instructions_async(workspace_root: &Path) -> Option<String> {
    let path = workspace_root.join(INSTRUCTION_FILE_NAME);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(_) => return None,
    };

    if bytes.len() > MAX_INSTRUCTION_FILE_BYTES {
        tracing::warn!(
            path = %path.display(),
            size = bytes.len(),
            max = MAX_INSTRUCTION_FILE_BYTES,
            "workspace instruction file exceeds size limit, truncating"
        );
        let truncated = &bytes[..MAX_INSTRUCTION_FILE_BYTES];
        let end = truncated
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(MAX_INSTRUCTION_FILE_BYTES);
        return Some(String::from_utf8_lossy(&bytes[..end]).into_owned());
    }

    tracing::info!(
        path = %path.display(),
        size = bytes.len(),
        "loaded workspace instruction file"
    );
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::discover_workspace_instructions;

    #[test]
    fn discovers_agents_md() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Use pytest for testing.").unwrap();

        let result = discover_workspace_instructions(dir.path());

        assert_eq!(result.as_deref(), Some("Use pytest for testing."));
    }

    #[test]
    fn ignores_non_agents_instruction_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "Conventions here.").unwrap();
        std::fs::create_dir(dir.path().join(".moa")).unwrap();
        std::fs::write(dir.path().join(".moa/instructions.md"), "MOA specific.").unwrap();

        let result = discover_workspace_instructions(dir.path());

        assert_eq!(result, None);
    }

    #[test]
    fn returns_none_when_no_file_exists() {
        let dir = tempdir().unwrap();

        assert!(discover_workspace_instructions(dir.path()).is_none());
    }

    #[test]
    fn truncates_oversized_files() {
        let dir = tempdir().unwrap();
        let large = "x\n".repeat(20_000);
        std::fs::write(dir.path().join("AGENTS.md"), &large).unwrap();

        let result = discover_workspace_instructions(dir.path()).unwrap();

        assert!(result.len() <= 32_768);
        assert!(result.ends_with('\n') || result.ends_with('x'));
    }
}
