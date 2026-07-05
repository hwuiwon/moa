//! Small crate-internal helpers shared across the skills modules.
//!
//! These collapse per-file copies of the same database, scope, JSON, and
//! completion-request boilerplate into one place.

use moa_core::{ActionRuleScope, MoaError, Result, RlsContext, TenantId};
#[cfg(feature = "skill-learning")]
use moa_core::{CompletionRequest, ContextMessage};
use serde_json::Value;
use sqlx::PgConnection;

/// Maps a sqlx failure to a storage error.
pub(crate) fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

/// Switches the current transaction to the row-level-security application role.
pub(crate) async fn set_app_role(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

/// Builds the tenant-scoped artifact scope for a tenant.
pub(crate) fn tenant_artifact_scope(tenant_id: TenantId) -> ActionRuleScope {
    ActionRuleScope::Tenant { tenant_id }
}

/// Derives the row-level-security context for an artifact scope.
pub(crate) fn artifact_scope_context(scope: &ActionRuleScope) -> RlsContext {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => RlsContext::tenant(*tenant_id),
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => RlsContext::contact(*tenant_id, *contact_id),
    }
}

/// Returns an empty JSON object value.
pub(crate) fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Builds a single-turn completion request from a system and user message.
///
/// Centralises the shared scaffold (no model override, empty tool set, default
/// sampling and metadata) used by the distillation and improvement prompts.
#[cfg(feature = "skill-learning")]
pub(crate) fn completion_request(
    system: impl Into<String>,
    user: impl Into<String>,
) -> CompletionRequest {
    CompletionRequest {
        model: None,
        messages: vec![ContextMessage::system(system), ContextMessage::user(user)],
        tools: Vec::new(),
        max_output_tokens: None,
        temperature: None,
        response_format: None,
        metadata: Default::default(),
    }
}
