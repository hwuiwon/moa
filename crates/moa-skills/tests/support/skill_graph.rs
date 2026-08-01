//! Shared graph-backed skill fixtures.
//!
//! Consolidates the tenant-scope, graph-store, and `DISTILLED_SKILL` helpers
//! previously duplicated across the `lessons`, `registry`, and `render` graph
//! test binaries. Each binary uses only a subset of these helpers, so the module
//! allows dead code rather than warning per binary.

#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use moa_artifacts::document::ArtifactStatus;
use moa_artifacts::registry::{
    ArtifactRegistry, NewArtifactDraft, NewArtifactFile, StoredArtifactRevision,
};
use moa_core::types::memory::RlsContext;
use moa_core::{error::MoaError, error::Result, types::action_policy::ActionRuleScope};
use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_memory_graph::PostgresGraphStore;
use moa_memory_types::MemoryScope;
use moa_skills::artifact::{SKILL_ARTIFACT_PATH, skill_artifact_document_from_package};
use moa_skills::package::SkillPackage;
use moa_test_support::fixtures::tenant_id_from_storage_partition;
use sqlx::PgConnection;

pub(crate) static GRAPH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn tenant_scope(storage_partition_id: &str) -> ActionRuleScope {
    ActionRuleScope::Tenant {
        tenant_id: tenant_id_from_storage_partition(storage_partition_id),
    }
}

pub(crate) fn memory_scope(storage_partition_id: &str) -> MemoryScope {
    MemoryScope::Tenant {
        tenant_id: tenant_id_from_storage_partition(storage_partition_id),
    }
}

pub(crate) fn graph_store(pool: &sqlx::PgPool, scope: &MemoryScope) -> PostgresGraphStore {
    static KMS: OnceLock<Arc<dyn KeyManagementProvider>> = OnceLock::new();
    let kms = KMS
        .get_or_init(|| Arc::new(LocalKmsProvider::new()))
        .clone();
    PostgresGraphStore::scoped_for_app_role(pool.clone(), RlsContext::from(scope.clone()), kms)
}

pub(crate) async fn set_app_role(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

/// Creates a skill draft and activates it so the tenant serves it.
///
/// The fixture drives the real validation, baseline, audit, and pointer
/// transaction, so it cannot make a skill serve that activation would refuse.
pub(crate) async fn serve_skill_package(
    pool: &sqlx::PgPool,
    scope: ActionRuleScope,
    package: SkillPackage,
) -> Result<uuid::Uuid> {
    let draft = draft_skill_package(pool, scope, package).await?;
    let release_scope = moa_artifacts::release::TenantScope::from_action_rule_scope(&scope)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    moa_artifacts::test_fixtures::activate_revision(
        pool,
        release_scope,
        moa_artifacts::release::ActivationTarget::SkillVisibility {
            artifact_uid: draft.artifact_uid,
        },
        draft.revision_uid,
    )
    .await
    .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    Ok(draft.revision_uid)
}

/// Creates one canonical draft through the generic artifact registry.
pub(crate) async fn draft_skill_package(
    pool: &sqlx::PgPool,
    scope: ActionRuleScope,
    package: SkillPackage,
) -> Result<StoredArtifactRevision> {
    let package = package.validate()?;
    let document = skill_artifact_document_from_package(&package, ArtifactStatus::Draft)?;
    let source_text = if let Some(file) = package
        .files
        .iter()
        .find(|file| file.path == SKILL_ARTIFACT_PATH)
    {
        file.content.clone()
    } else {
        document
            .to_yaml()
            .map_err(|error| MoaError::SerializationError(error.to_string()))?
            .into_bytes()
    };
    let files = package
        .files
        .iter()
        .map(|file| NewArtifactFile {
            path: file.path.clone(),
            content: file.content.clone(),
            content_type: file.content_type.clone(),
            executable: file.executable,
        })
        .collect::<Vec<_>>();
    ArtifactRegistry::new(pool.clone())
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: &source_text,
                files: &files,
            },
        )
        .await
}

pub(crate) async fn purge_test_skill_name(
    store: &moa_session::PostgresSessionStore,
    skill_name: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM moa.artifact WHERE kind = 'skill' AND name = $1")
        .bind(skill_name)
        .execute(store.pool())
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

pub(crate) fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

pub(crate) const DISTILLED_SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate and fix OAuth refresh-token bugs"
compatibility: "Requires local repo access"
allowed-tools: bash file_read file_search
metadata:
  moa-version: "1.0"
  moa-tags: "oauth, auth, debugging"
  moa-estimated-tokens: "900"
---

# Debug OAuth refresh

1. Reproduce the bug.
2. Inspect the refresh-token path.
3. Verify the fix.
"#;

pub(crate) const IMPROVED_SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate and fix OAuth refresh-token bugs with regression checks"
compatibility: "Requires local repo access"
allowed-tools: bash file_read file_search file_write
metadata:
  moa-version: "1.0"
  moa-tags: "oauth, auth, debugging"
  moa-estimated-tokens: "950"
---

# Debug OAuth refresh

1. Reproduce the bug.
2. Add a regression test before changing code.
3. Inspect the refresh-token path.
4. Verify the fix and the new test.
"#;
