//! Tool input normalization, action-review summaries, and local path helpers.

use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use moa_core::shell::split_shell_chain;
use moa_core::{
    ActionReviewField, ActionReviewFileDiff, MoaError, Result, ToolDiffStrategy, ToolInputShape,
    ToolInvocation,
};
use serde_json::Value;
use tokio::fs;

use crate::tools::file_read::resolve_sandbox_path;
use crate::tools::sandbox_descriptor::{
    SandboxActionPattern, SandboxReviewPreviewMetadata, sandbox_tool_descriptor,
};
use crate::tools::str_replace::plan_str_replace;

/// Recognized login shell wrapper prefixes used by the bash tool.
const SHELL_WRAPPERS: &[(&str, &[&str])] = &[
    ("zsh", &["-lc", "-c"]),
    ("bash", &["-lc", "-c"]),
    ("sh", &["-c"]),
];

const BARE_SHELL_NAMES: &[&str] = &["zsh", "bash", "sh", "dash", "fish"];
const MAX_REVIEW_DIFF_CHARS: usize = 16_384;

pub(super) fn normalized_input_for(
    tool_name: &str,
    input_shape: ToolInputShape,
    input: &Value,
) -> Result<String> {
    let input_shape = sandbox_tool_descriptor(tool_name)
        .map(|descriptor| descriptor.normalization.input_shape)
        .unwrap_or(input_shape);
    normalized_input_for_shape(input_shape, input)
}

fn normalized_input_for_shape(input_shape: ToolInputShape, input: &Value) -> Result<String> {
    let value = match input_shape {
        ToolInputShape::Command => required_string_field(input, "cmd")?,
        ToolInputShape::Path => required_string_field(input, "path")?,
        ToolInputShape::Pattern => required_string_field(input, "pattern")?,
        ToolInputShape::Query => required_string_field(input, "query")?,
        ToolInputShape::Url => required_string_field(input, "url")?,
        ToolInputShape::Json => serde_json::to_string(input)?,
    };

    Ok(value.trim().to_string())
}

pub(super) fn summary_for(
    tool_name: &str,
    input_shape: ToolInputShape,
    input: &Value,
    normalized_input: &str,
) -> String {
    let input_shape = sandbox_tool_descriptor(tool_name)
        .map(|descriptor| descriptor.normalization.input_shape)
        .unwrap_or(input_shape);
    match input_shape {
        ToolInputShape::Command => normalized_input.to_string(),
        ToolInputShape::Path => {
            if let Some(content) = input.get("content").and_then(Value::as_str) {
                format!(
                    "Path: {normalized_input} | {} chars",
                    content.chars().count()
                )
            } else {
                format!("Path: {normalized_input}")
            }
        }
        ToolInputShape::Pattern => format!("Pattern: {normalized_input}"),
        ToolInputShape::Query => format!("Query: {normalized_input}"),
        ToolInputShape::Url => format!("URL: {normalized_input}"),
        ToolInputShape::Json => normalized_input.to_string(),
    }
}

/// Attempts to extract the inner command from a recognized shell wrapper invocation.
///
/// Only one wrapper layer is unwrapped. Unrecognized or malformed wrapper forms return `None`.
pub(super) fn unwrap_shell_wrapper(normalized_input: &str) -> Option<String> {
    let tokens = shell_words::split(normalized_input).ok()?;

    for (shell, flags) in SHELL_WRAPPERS {
        let inner = match tokens.as_slice() {
            [command, flag, inner]
                if command == shell && flags.iter().any(|candidate| flag == candidate) =>
            {
                inner
            }
            [command, login_flag, command_flag, inner]
                if command == shell
                    && *login_flag == "-l"
                    && *command_flag == "-c"
                    && flags.contains(&"-lc") =>
            {
                inner
            }
            _ => continue,
        };

        return Some(inner.clone());
    }

    None
}

pub(super) fn action_pattern_for(
    tool_name: &str,
    input_shape: ToolInputShape,
    normalized_input: &str,
) -> String {
    if let Some(descriptor) = sandbox_tool_descriptor(tool_name) {
        return match descriptor.normalization.action_pattern {
            SandboxActionPattern::NormalizedInput => normalized_input.to_string(),
            SandboxActionPattern::ShellFirstCommand => shell_action_pattern_for(normalized_input),
        };
    }
    action_pattern_for_shape(input_shape, normalized_input)
}

fn action_pattern_for_shape(input_shape: ToolInputShape, normalized_input: &str) -> String {
    if matches!(input_shape, ToolInputShape::Command) {
        return shell_action_pattern_for(normalized_input);
    }

    normalized_input.to_string()
}

fn shell_action_pattern_for(normalized_input: &str) -> String {
    let effective_command =
        unwrap_shell_wrapper(normalized_input).unwrap_or_else(|| normalized_input.to_string());
    let sub_commands = split_shell_chain(&effective_command);
    let target = sub_commands
        .first()
        .map(std::string::String::as_str)
        .unwrap_or(effective_command.as_str());
    let tokens = shell_words::split(target).unwrap_or_default();
    if let Some(command) = tokens.first() {
        if BARE_SHELL_NAMES.contains(&command.as_str()) {
            return normalized_input.to_string();
        }
        return if tokens.len() == 1 {
            command.clone()
        } else {
            format!("{command} *")
        };
    }

    normalized_input.to_string()
}

pub(super) fn review_fields_for(
    sandbox_root: Option<&Path>,
    input_shape: ToolInputShape,
    invocation: &ToolInvocation,
) -> Vec<ActionReviewField> {
    if let Some(descriptor) = sandbox_tool_descriptor(&invocation.name) {
        return sandbox_review_fields_for(sandbox_root, descriptor.review_preview, invocation);
    }

    match input_shape {
        ToolInputShape::Command => {
            let command = invocation
                .input
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut fields = vec![ActionReviewField {
                label: "Command".to_string(),
                value: command,
            }];
            if let Some(sandbox_root) = sandbox_root {
                fields.push(ActionReviewField {
                    label: "Working dir".to_string(),
                    value: sandbox_root.display().to_string(),
                });
            }
            fields
        }
        ToolInputShape::Path => {
            let mut fields = single_review_field("Path", &invocation.input, "path");
            if invocation.name == "file_write" {
                let content_len = invocation
                    .input
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|content| content.chars().count())
                    .unwrap_or_default();
                fields.push(ActionReviewField {
                    label: "Content".to_string(),
                    value: format!("{content_len} chars"),
                });
            }
            if invocation.name == "str_replace" {
                let old_len = invocation
                    .input
                    .get("old_str")
                    .and_then(Value::as_str)
                    .map(|content| content.chars().count())
                    .unwrap_or_default();
                let new_len = invocation
                    .input
                    .get("new_str")
                    .and_then(Value::as_str)
                    .map(|content| content.chars().count())
                    .unwrap_or_default();
                fields.push(ActionReviewField {
                    label: "Old string".to_string(),
                    value: format!("{old_len} chars"),
                });
                fields.push(ActionReviewField {
                    label: "New string".to_string(),
                    value: format!("{new_len} chars"),
                });
                if let Some(insert_after_line) = invocation
                    .input
                    .get("insert_after_line")
                    .and_then(Value::as_u64)
                {
                    fields.push(ActionReviewField {
                        label: "Insert after line".to_string(),
                        value: insert_after_line.to_string(),
                    });
                }
            }
            fields
        }
        ToolInputShape::Pattern => single_review_field("Pattern", &invocation.input, "pattern"),
        ToolInputShape::Query => single_review_field("Query", &invocation.input, "query"),
        ToolInputShape::Url => single_review_field("URL", &invocation.input, "url"),
        ToolInputShape::Json => serde_json::to_string_pretty(&invocation.input)
            .map(|value| {
                vec![ActionReviewField {
                    label: "Input".to_string(),
                    value,
                }]
            })
            .unwrap_or_default(),
    }
}

fn sandbox_review_fields_for(
    sandbox_root: Option<&Path>,
    preview: SandboxReviewPreviewMetadata,
    invocation: &ToolInvocation,
) -> Vec<ActionReviewField> {
    match preview {
        SandboxReviewPreviewMetadata::Command {
            command_field,
            command_label,
            working_dir_label,
        } => {
            let command = invocation
                .input
                .get(command_field)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut fields = vec![ActionReviewField {
                label: command_label.to_string(),
                value: command,
            }];
            if let Some(sandbox_root) = sandbox_root {
                fields.push(ActionReviewField {
                    label: working_dir_label.to_string(),
                    value: sandbox_root.display().to_string(),
                });
            }
            fields
        }
        SandboxReviewPreviewMetadata::SingleField { field, label } => {
            single_review_field(label, &invocation.input, field)
        }
        SandboxReviewPreviewMetadata::FileWrite {
            path_field,
            content_field,
        } => {
            let mut fields = single_review_field("Path", &invocation.input, path_field);
            let content_len = invocation
                .input
                .get(content_field)
                .and_then(Value::as_str)
                .map(|content| content.chars().count())
                .unwrap_or_default();
            fields.push(ActionReviewField {
                label: "Content".to_string(),
                value: format!("{content_len} chars"),
            });
            fields
        }
        SandboxReviewPreviewMetadata::StrReplace {
            path_field,
            old_field,
            new_field,
            insert_after_line_field,
        } => {
            let mut fields = single_review_field("Path", &invocation.input, path_field);
            let old_len = invocation
                .input
                .get(old_field)
                .and_then(Value::as_str)
                .map(|content| content.chars().count())
                .unwrap_or_default();
            let new_len = invocation
                .input
                .get(new_field)
                .and_then(Value::as_str)
                .map(|content| content.chars().count())
                .unwrap_or_default();
            fields.push(ActionReviewField {
                label: "Old string".to_string(),
                value: format!("{old_len} chars"),
            });
            fields.push(ActionReviewField {
                label: "New string".to_string(),
                value: format!("{new_len} chars"),
            });
            if let Some(insert_after_line) = invocation
                .input
                .get(insert_after_line_field)
                .and_then(Value::as_u64)
            {
                fields.push(ActionReviewField {
                    label: "Insert after line".to_string(),
                    value: insert_after_line.to_string(),
                });
            }
            fields
        }
    }
}

fn single_review_field(label: &str, input: &Value, field: &str) -> Vec<ActionReviewField> {
    let value = input
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    vec![ActionReviewField {
        label: label.to_string(),
        value,
    }]
}

pub(super) async fn review_diffs_for(
    sandbox_root: Option<&Path>,
    diff_strategy: ToolDiffStrategy,
    invocation: &ToolInvocation,
) -> Result<Vec<ActionReviewFileDiff>> {
    let Some(sandbox_root) = sandbox_root else {
        return Ok(Vec::new());
    };
    match diff_strategy {
        ToolDiffStrategy::None => Ok(Vec::new()),
        ToolDiffStrategy::FileWrite => {
            let Some(path) = invocation.input.get("path").and_then(Value::as_str) else {
                return Ok(Vec::new());
            };
            let Some(content) = invocation.input.get("content").and_then(Value::as_str) else {
                return Ok(Vec::new());
            };

            let file_path = resolve_sandbox_path(sandbox_root, path)?;
            let before = read_existing_text_file(&file_path)
                .await?
                .unwrap_or_default();

            Ok(vec![ActionReviewFileDiff {
                path: path.to_string(),
                before: cap_review_text(before),
                after: cap_review_text(content.to_string()),
                language_hint: language_hint_for_path(path),
            }])
        }
        ToolDiffStrategy::StrReplace => {
            let Some(path) = invocation.input.get("path").and_then(Value::as_str) else {
                return Ok(Vec::new());
            };
            let file_path = resolve_sandbox_path(sandbox_root, path)?;
            let before = read_existing_text_file(&file_path).await?;
            let input = serde_json::to_string(&invocation.input)?;
            let planned = match plan_str_replace(input.as_str(), before.as_deref(), path, 4) {
                Ok(planned) => planned,
                Err(_) => {
                    let fallback_before = invocation
                        .input
                        .get("old_str")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let fallback_after = invocation
                        .input
                        .get("new_str")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    crate::tools::str_replace::PlannedStrReplace {
                        updated_content: String::new(),
                        preview_before: fallback_before,
                        preview_after: fallback_after,
                    }
                }
            };

            Ok(vec![ActionReviewFileDiff {
                path: path.to_string(),
                before: cap_review_text(planned.preview_before),
                after: cap_review_text(planned.preview_after),
                language_hint: language_hint_for_path(path),
            }])
        }
    }
}

fn cap_review_text(value: String) -> String {
    let mut chars = value.chars();
    let capped = chars
        .by_ref()
        .take(MAX_REVIEW_DIFF_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{capped}\n\n[preview truncated at {MAX_REVIEW_DIFF_CHARS} chars]")
    } else {
        capped
    }
}

async fn read_existing_text_file(path: &Path) -> Result<Option<String>> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn language_hint_for_path(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(ToOwned::to_owned)
}

fn required_string_field(input: &Value, field: &str) -> Result<String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            MoaError::ValidationError(format!(
                "tool input is missing required string field `{field}`"
            ))
        })
}

pub(super) fn expand_local_path(path: &str) -> Result<PathBuf> {
    if let Some(relative) = path.strip_prefix("~/") {
        let home = env::var("HOME").map_err(|_| MoaError::HomeDirectoryNotFound)?;
        return Ok(PathBuf::from(home).join(relative));
    }

    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use moa_core::ToolInputShape;

    use super::{action_pattern_for_shape, unwrap_shell_wrapper};

    #[test]
    fn unwrap_shell_wrapper_recognizes_supported_forms() {
        let cases = [
            (
                r#"zsh -lc "cd server && rg -n 'class CallViewSet' .""#,
                "cd server && rg -n 'class CallViewSet' .",
            ),
            (r#"zsh -l -c "npm test""#, "npm test"),
            (r#"bash -lc "cargo test""#, "cargo test"),
            (r#"bash -c "npm test""#, "npm test"),
            (r#"sh -c "pwd""#, "pwd"),
        ];

        for (input, expected) in cases {
            assert_eq!(unwrap_shell_wrapper(input).as_deref(), Some(expected));
        }
    }

    #[test]
    fn no_unwrap_for_plain_command() {
        assert!(unwrap_shell_wrapper("npm test").is_none());
        assert!(unwrap_shell_wrapper("rg -n pattern .").is_none());
    }

    #[test]
    fn action_pattern_unwraps_zsh_wrapper() {
        let pattern = action_pattern_for_shape(
            ToolInputShape::Command,
            r#"zsh -lc "cd server && rg -n 'class' .""#,
        );

        assert_eq!(pattern, "cd *");
        assert_ne!(pattern, "zsh *");
    }

    #[test]
    fn action_pattern_simple_command() {
        let pattern = action_pattern_for_shape(ToolInputShape::Command, "npm test");
        assert_eq!(pattern, "npm *");
    }

    #[test]
    fn action_pattern_single_token() {
        let pattern = action_pattern_for_shape(ToolInputShape::Command, "pwd");
        assert_eq!(pattern, "pwd");
    }

    #[test]
    fn action_pattern_nested_shell_not_recursed() {
        let input = r#"bash -c "bash -c 'rm -rf /'""#;
        let pattern = action_pattern_for_shape(ToolInputShape::Command, input);

        assert_eq!(pattern, input);
        assert!(!pattern.starts_with("rm"));
    }

    #[test]
    fn action_pattern_chained_inner_uses_first_subcommand() {
        let pattern = action_pattern_for_shape(
            ToolInputShape::Command,
            r#"zsh -lc "npm install && npm test""#,
        );

        assert_eq!(pattern, "npm *");
    }

    #[test]
    fn action_pattern_malformed_wrapper_falls_back_to_full_input() {
        let input = r#"zsh -lc "unterminated"#;
        let pattern = action_pattern_for_shape(ToolInputShape::Command, input);

        assert_eq!(pattern, input);
    }
}
