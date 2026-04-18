//! Shared output helpers for text-editing tools.

use std::time::Duration;

use moa_core::{ToolOutput, compute_unified_diff};

const DIFF_CONTEXT_LINES: usize = 3;

/// Snapshot of a file before a write operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExistingFileContent {
    /// The file did not exist before the write.
    Missing,
    /// The file existed and contained UTF-8 text.
    Text(String),
    /// The file existed but did not contain UTF-8 text.
    Binary,
}

/// Builds the output for a write or overwrite operation.
pub(crate) fn build_file_write_output(
    display_path: &str,
    existing: &ExistingFileContent,
    new_content: &str,
    duration: Duration,
) -> ToolOutput {
    match existing {
        ExistingFileContent::Missing => ToolOutput::text(
            format!(
                "[new file created: {display_path}, {} lines]",
                count_lines(new_content)
            ),
            duration,
        ),
        ExistingFileContent::Binary => ToolOutput::text(
            format!(
                "[binary file written: {display_path}, {} bytes]",
                new_content.len()
            ),
            duration,
        ),
        ExistingFileContent::Text(before) => {
            build_text_edit_output(display_path, before, new_content, duration)
        }
    }
}

/// Builds the output for a text edit operation using a unified diff.
pub(crate) fn build_text_edit_output(
    display_path: &str,
    before: &str,
    after: &str,
    duration: Duration,
) -> ToolOutput {
    let diff = compute_unified_diff(display_path, before, after, DIFF_CONTEXT_LINES);
    if diff.trim().is_empty() {
        ToolOutput::text(format!("[no changes written: {display_path}]"), duration)
    } else {
        ToolOutput::text(diff, duration)
    }
}

fn count_lines(content: &str) -> usize {
    if content.is_empty() {
        0
    } else {
        content.lines().count().max(1)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ExistingFileContent, build_file_write_output, build_text_edit_output};

    #[test]
    fn new_file_write_returns_creation_notice() {
        let output = build_file_write_output(
            "src/main.rs",
            &ExistingFileContent::Missing,
            "fn main() {}\n",
            Duration::default(),
        );

        assert_eq!(output.to_text(), "[new file created: src/main.rs, 1 lines]");
    }

    #[test]
    fn binary_overwrite_returns_binary_notice() {
        let output = build_file_write_output(
            "assets/logo.bin",
            &ExistingFileContent::Binary,
            "text replacement",
            Duration::default(),
        );

        assert_eq!(
            output.to_text(),
            "[binary file written: assets/logo.bin, 16 bytes]"
        );
    }

    #[test]
    fn text_edit_output_uses_unified_diff() {
        let output = build_text_edit_output(
            "src/lib.rs",
            "fn demo() {\n    alpha();\n}\n",
            "fn demo() {\n    beta();\n}\n",
            Duration::default(),
        );

        let rendered = output.to_text();
        assert!(rendered.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n"));
        assert!(rendered.contains("-    alpha();"));
        assert!(rendered.contains("+    beta();"));
    }
}
