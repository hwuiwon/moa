//! `MEMORY.md` compilation and `_log.md` append-only helpers.

use std::path::Path;

use chrono::{DateTime, Utc};
use moa_core::{MemoryPath, MemoryScope, PageSummary, PageType, Result, SessionId};
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// Maximum number of lines loaded from `MEMORY.md`.
pub const MAX_INDEX_LINES: usize = 200;
/// Maximum number of bytes loaded from `MEMORY.md`.
pub const MAX_INDEX_BYTES: usize = 25_000;
/// Standard index filename for a memory scope.
pub const INDEX_FILENAME: &str = "MEMORY.md";
/// Append-only log filename for a memory scope.
pub const LOG_FILENAME: &str = "_log.md";

/// Single page-level change recorded in `_log.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChange {
    /// Change verb such as `Created`, `Updated`, `Deleted`, or `Merged`.
    pub action: String,
    /// Logical wiki path affected by the operation.
    pub path: MemoryPath,
    /// Optional human-readable detail appended after the path.
    pub detail: Option<String>,
}

/// Single append-only memory log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Timestamp recorded in the markdown heading.
    pub timestamp: DateTime<Utc>,
    /// Stable operation label such as `ingest` or `consolidation`.
    pub operation: String,
    /// Short human-readable description.
    pub description: String,
    /// Page-level changes emitted by the operation.
    pub changes: Vec<LogChange>,
    /// Optional session identifier that initiated the change.
    pub brain_session: Option<SessionId>,
}

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

/// Loads the append-only `_log.md` file for a scope.
pub async fn load_log_file(scope_root: &Path) -> Result<String> {
    let path = scope_root.join(LOG_FILENAME);
    match fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

/// Appends a rendered markdown entry to `_log.md`.
pub async fn append_log_entry(scope_root: &Path, entry: &LogEntry) -> Result<()> {
    fs::create_dir_all(scope_root).await?;
    let path = scope_root.join(LOG_FILENAME);
    let existing = load_log_file(scope_root).await?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;

    if !existing.trim().is_empty() {
        file.write_all(b"\n\n").await?;
    }
    file.write_all(render_log_entry(entry).as_bytes()).await?;
    Ok(())
}

/// Returns the latest timestamp logged for an operation.
pub fn last_operation_timestamp(content: &str, operation: &str) -> Option<DateTime<Utc>> {
    let mut latest = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("## [") {
            continue;
        }

        let Some((timestamp_raw, remainder)) = trimmed[4..].split_once(']') else {
            continue;
        };
        let Some(actual_operation) = remainder.trim().split('|').next() else {
            continue;
        };
        if actual_operation.trim() != operation {
            continue;
        }

        let parsed = DateTime::parse_from_rfc3339(timestamp_raw.trim_start_matches('['))
            .ok()?
            .with_timezone(&Utc);
        latest = Some(parsed);
    }

    latest
}

/// Compiles a concise `MEMORY.md` document from current page summaries.
pub fn compile_index(scope: &MemoryScope, pages: &[PageSummary]) -> String {
    let mut sorted = pages
        .iter()
        .filter(|page| {
            !matches!(
                page.page_type,
                PageType::Index | PageType::Log | PageType::Schema
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    sorted.sort_by_key(|page| std::cmp::Reverse(page.updated));

    let heading = match scope {
        MemoryScope::Global => "# Global Memory".to_string(),
        MemoryScope::Workspace { workspace_id } => {
            format!("# Workspace: {}", workspace_id.as_str())
        }
        MemoryScope::User {
            workspace_id,
            user_id,
        } => format!(
            "# User Memory: {} in {}",
            user_id.as_str(),
            workspace_id.as_str()
        ),
    };

    let mut lines = vec![
        heading,
        "Auto-generated quick reference for this memory scope.".to_string(),
        String::new(),
        "## Recent pages".to_string(),
    ];

    for page in sorted {
        if lines.len() >= MAX_INDEX_LINES {
            break;
        }

        let line = format!(
            "- [[{}]] | {} | {:?} | {}",
            page.path.as_str(),
            page.title,
            page.page_type,
            page.updated.format("%Y-%m-%d")
        );
        let prospective = if lines.is_empty() {
            line.len()
        } else {
            lines.join("\n").len() + 1 + line.len()
        };
        if prospective > MAX_INDEX_BYTES {
            break;
        }
        lines.push(line);
    }

    lines.join("\n")
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

fn render_log_entry(entry: &LogEntry) -> String {
    let mut lines = vec![format!(
        "## [{}] {} | {}",
        entry.timestamp.to_rfc3339(),
        entry.operation,
        entry.description
    )];

    for change in &entry.changes {
        if let Some(detail) = &change.detail {
            lines.push(format!(
                "- {}: {} ({detail})",
                change.action,
                change.path.as_str()
            ));
        } else {
            lines.push(format!("- {}: {}", change.action, change.path.as_str()));
        }
    }

    if let Some(session_id) = &entry.brain_session {
        lines.push(format!("- Brain: {}", session_id));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::{ConfidenceLevel, MemoryScope, PageSummary, PageType, WorkspaceId};

    use super::{
        LogChange, LogEntry, MAX_INDEX_LINES, append_log_entry, compile_index, load_log_file,
        truncate_index_content,
    };

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

    #[test]
    fn compiled_index_stays_within_line_budget() {
        let updated = Utc.with_ymd_and_hms(2026, 4, 9, 16, 45, 0).unwrap();
        let pages = (0..240)
            .map(|index| PageSummary {
                path: format!("topics/page-{index}.md").into(),
                title: format!("Page {index}"),
                page_type: PageType::Topic,
                confidence: ConfidenceLevel::High,
                updated,
            })
            .collect::<Vec<_>>();

        let compiled = compile_index(
            &MemoryScope::Workspace {
                workspace_id: WorkspaceId::new("ws1"),
            },
            &pages,
        );

        assert!(compiled.lines().count() <= MAX_INDEX_LINES);
    }

    #[tokio::test]
    async fn append_only_log_keeps_prior_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        let entry = LogEntry {
            timestamp: Utc.with_ymd_and_hms(2026, 4, 9, 10, 0, 0).unwrap(),
            operation: "ingest".to_string(),
            description: "Added source".to_string(),
            changes: vec![LogChange {
                action: "Created".to_string(),
                path: "sources/test.md".into(),
                detail: None,
            }],
            brain_session: None,
        };

        append_log_entry(&root, &entry).await.unwrap();
        append_log_entry(&root, &entry).await.unwrap();

        let log = load_log_file(&root).await.unwrap();
        assert_eq!(log.matches("## [").count(), 2);
    }
}
