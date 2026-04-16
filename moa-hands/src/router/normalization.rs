//! Tool input normalization, approval summaries, and local path helpers.

use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use moa_core::shell::split_shell_chain;
use moa_core::{
    ApprovalField, ApprovalFileDiff, MoaError, Result, ToolDiffStrategy, ToolInputShape,
    ToolInvocation,
};
use serde_json::Value;
use tokio::fs;

use crate::tools::file_read::resolve_sandbox_path;
use crate::tools::str_replace::plan_str_replace;

/// Recognized login shell wrapper prefixes used by the bash tool.
const SHELL_WRAPPERS: &[(&str, &[&str])] = &[
    ("zsh", &["-lc", "-c"]),
    ("bash", &["-lc", "-c"]),
    ("sh", &["-c"]),
];

const BARE_SHELL_NAMES: &[&str] = &["zsh", "bash", "sh", "dash", "fish"];

pub(super) fn normalized_input_for(input_shape: ToolInputShape, input: &Value) -> Result<String> {
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
    input_shape: ToolInputShape,
    input: &Value,
    normalized_input: &str,
) -> String {
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

pub(super) fn approval_pattern_for(input_shape: ToolInputShape, normalized_input: &str) -> String {
    if matches!(input_shape, ToolInputShape::Command) {
        let effective_command =
            unwrap_shell_wrapper(normalized_input).unwrap_or_else(|| normalized_input.to_string());
        let sub_commands = split_shell_chain(&effective_command);
        let target = sub_commands
            .first()
            .map(|sub_command| sub_command.as_str())
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
    }

    normalized_input.to_string()
}

pub(super) fn approval_fields_for(
    sandbox_root: Option<&Path>,
    input_shape: ToolInputShape,
    invocation: &ToolInvocation,
) -> Vec<ApprovalField> {
    match input_shape {
        ToolInputShape::Command => {
            let command = invocation
                .input
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut fields = vec![ApprovalField {
                label: "Command".to_string(),
                value: command,
            }];
            if let Some(sandbox_root) = sandbox_root {
                fields.push(ApprovalField {
                    label: "Working dir".to_string(),
                    value: sandbox_root.display().to_string(),
                });
            }
            fields
        }
        ToolInputShape::Path => {
            let mut fields = single_approval_field("Path", &invocation.input, "path");
            if invocation.name == "file_write" {
                let content_len = invocation
                    .input
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|content| content.chars().count())
                    .unwrap_or_default();
                fields.push(ApprovalField {
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
                fields.push(ApprovalField {
                    label: "Old string".to_string(),
                    value: format!("{old_len} chars"),
                });
                fields.push(ApprovalField {
                    label: "New string".to_string(),
                    value: format!("{new_len} chars"),
                });
                if let Some(insert_after_line) = invocation
                    .input
                    .get("insert_after_line")
                    .and_then(Value::as_u64)
                {
                    fields.push(ApprovalField {
                        label: "Insert after line".to_string(),
                        value: insert_after_line.to_string(),
                    });
                }
            }
            fields
        }
        ToolInputShape::Pattern => single_approval_field("Pattern", &invocation.input, "pattern"),
        ToolInputShape::Query => single_approval_field("Query", &invocation.input, "query"),
        ToolInputShape::Url => single_approval_field("URL", &invocation.input, "url"),
        ToolInputShape::Json => serde_json::to_string_pretty(&invocation.input)
            .map(|value| {
                vec![ApprovalField {
                    label: "Input".to_string(),
                    value,
                }]
            })
            .unwrap_or_default(),
    }
}

fn single_approval_field(label: &str, input: &Value, field: &str) -> Vec<ApprovalField> {
    let value = input
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    vec![ApprovalField {
        label: label.to_string(),
        value,
    }]
}

pub(super) async fn approval_diffs_for(
    sandbox_root: Option<&Path>,
    diff_strategy: ToolDiffStrategy,
    invocation: &ToolInvocation,
) -> Result<Vec<ApprovalFileDiff>> {
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

            Ok(vec![ApprovalFileDiff {
                path: path.to_string(),
                before,
                after: content.to_string(),
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
                        message: String::new(),
                        preview_before: fallback_before,
                        preview_after: fallback_after,
                    }
                }
            };

            Ok(vec![ApprovalFileDiff {
                path: path.to_string(),
                before: planned.preview_before,
                after: planned.preview_after,
                language_hint: language_hint_for_path(path),
            }])
        }
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

    use super::{approval_pattern_for, unwrap_shell_wrapper};

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
    fn approval_pattern_unwraps_zsh_wrapper() {
        let pattern = approval_pattern_for(
            ToolInputShape::Command,
            r#"zsh -lc "cd server && rg -n 'class' .""#,
        );

        assert_eq!(pattern, "cd *");
        assert_ne!(pattern, "zsh *");
    }

    #[test]
    fn approval_pattern_simple_command() {
        let pattern = approval_pattern_for(ToolInputShape::Command, "npm test");
        assert_eq!(pattern, "npm *");
    }

    #[test]
    fn approval_pattern_single_token() {
        let pattern = approval_pattern_for(ToolInputShape::Command, "pwd");
        assert_eq!(pattern, "pwd");
    }

    #[test]
    fn approval_pattern_nested_shell_not_recursed() {
        let input = r#"bash -c "bash -c 'rm -rf /'""#;
        let pattern = approval_pattern_for(ToolInputShape::Command, input);

        assert_eq!(pattern, input);
        assert!(!pattern.starts_with("rm"));
    }

    #[test]
    fn approval_pattern_chained_inner_uses_first_subcommand() {
        let pattern = approval_pattern_for(
            ToolInputShape::Command,
            r#"zsh -lc "npm install && npm test""#,
        );

        assert_eq!(pattern, "npm *");
    }

    #[test]
    fn approval_pattern_malformed_wrapper_falls_back_to_full_input() {
        let input = r#"zsh -lc "unterminated"#;
        let pattern = approval_pattern_for(ToolInputShape::Command, input);

        assert_eq!(pattern, input);
    }
}
