//! Tool input normalization, approval summaries, and local path helpers.

use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use moa_core::{
    ApprovalField, ApprovalFileDiff, MoaError, Result, ToolDiffStrategy, ToolInputShape,
    ToolInvocation,
};
use serde_json::Value;
use tokio::fs;

use crate::tools::file_read::resolve_sandbox_path;

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

pub(super) fn approval_pattern_for(input_shape: ToolInputShape, normalized_input: &str) -> String {
    if matches!(input_shape, ToolInputShape::Command) {
        let tokens = shell_words::split(normalized_input).unwrap_or_default();
        if let Some(command) = tokens.first() {
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
    if !matches!(diff_strategy, ToolDiffStrategy::FileWrite) {
        return Ok(Vec::new());
    }

    let Some(sandbox_root) = sandbox_root else {
        return Ok(Vec::new());
    };
    let Some(path) = invocation.input.get("path").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    let Some(content) = invocation.input.get("content").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    let file_path = resolve_sandbox_path(sandbox_root, path)?;
    let before = read_existing_text_file(&file_path).await?;

    Ok(vec![ApprovalFileDiff {
        path: path.to_string(),
        before,
        after: content.to_string(),
        language_hint: language_hint_for_path(path),
    }])
}

async fn read_existing_text_file(path: &Path) -> Result<String> {
    match fs::read(path).await {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
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
