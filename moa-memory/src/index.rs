//! `MEMORY.md` loading and truncation helpers.

use std::path::Path;

use moa_core::Result;
use tokio::fs;

/// Maximum number of lines loaded from `MEMORY.md`.
pub const MAX_INDEX_LINES: usize = 200;
/// Maximum number of bytes loaded from `MEMORY.md`.
pub const MAX_INDEX_BYTES: usize = 25_000;
/// Standard index filename for a memory scope.
pub const INDEX_FILENAME: &str = "MEMORY.md";

/// Loads a scope index file and truncates it for prompt inclusion.
pub async fn load_index_file(path: &Path) -> Result<String> {
    match fs::read_to_string(path).await {
        Ok(content) => Ok(truncate_index_content(&content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

/// Writes the raw `MEMORY.md` content for a scope.
pub async fn write_index_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, content).await?;
    Ok(())
}

/// Truncates index content to the configured line and byte limits.
pub fn truncate_index_content(content: &str) -> String {
    let mut truncated = String::new();

    for (line_count, line) in content.lines().enumerate() {
        if line_count >= MAX_INDEX_LINES {
            break;
        }

        let line_len = line.len();
        let separator_len = usize::from(!truncated.is_empty());
        if truncated.len() + line_len + separator_len > MAX_INDEX_BYTES {
            break;
        }

        if !truncated.is_empty() {
            truncated.push('\n');
        }
        truncated.push_str(line);
    }

    truncated
}

#[cfg(test)]
mod tests {
    use super::{MAX_INDEX_LINES, truncate_index_content};

    #[test]
    fn memory_index_truncates_to_200_lines() {
        let content = (0..220)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");

        let truncated = truncate_index_content(&content);

        assert_eq!(truncated.lines().count(), MAX_INDEX_LINES);
        assert!(truncated.contains("line 199"));
        assert!(!truncated.contains("line 200"));
    }
}
