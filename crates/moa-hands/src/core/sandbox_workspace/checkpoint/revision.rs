//! Provider-neutral validation for durable workspace revisions.

use moa_core::{
    error::{MoaError, Result},
    types::{
        identifiers::WorkspaceCheckpointId,
        sandbox_workspace::{WorkspaceRevisionRef, WorkspaceStorageOperation},
    },
};

use super::archive::CHECKPOINT_ARCHIVE_FORMAT_VERSION;

/// Returns the committed revision required by a provider operation.
pub(crate) fn required_current_revision<'a>(
    operation: &'a WorkspaceStorageOperation,
    provider: &str,
) -> Result<&'a WorkspaceRevisionRef> {
    operation.binding.current_revision.as_ref().ok_or_else(|| {
        MoaError::ValidationError(format!(
            "{provider} operation requires a committed checkpoint revision"
        ))
    })
}

/// Builds the next revision after validating the exact committed parent.
pub(crate) fn next_workspace_revision(
    operation: &WorkspaceStorageOperation,
    parent_revision: Option<&WorkspaceRevisionRef>,
    checkpoint_id: WorkspaceCheckpointId,
    provider: Option<&str>,
) -> Result<WorkspaceRevisionRef> {
    let generation = match (operation.binding.current_revision.as_ref(), parent_revision) {
        (None, None) => 1,
        (Some(current), Some(parent)) if current == parent && parent.generation > 0 => {
            parent.generation.checked_add(1).ok_or_else(|| {
                MoaError::ValidationError(provider_message(
                    provider,
                    "workspace revision generation overflow",
                ))
            })?
        }
        _ => {
            return Err(MoaError::ValidationError(provider_message(
                provider,
                "checkpoint parent does not match the exact committed head; generation zero must use no revision",
            )));
        }
    };
    Ok(WorkspaceRevisionRef {
        checkpoint_id,
        generation,
        format_version: CHECKPOINT_ARCHIVE_FORMAT_VERSION,
    })
}

fn provider_message(provider: Option<&str>, message: &str) -> String {
    provider.map_or_else(
        || message.to_string(),
        |provider| format!("{provider} {message}"),
    )
}
