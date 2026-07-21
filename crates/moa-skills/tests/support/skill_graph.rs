//! Shared graph-backed skill fixtures.
//!
//! Consolidates the tenant-scope, graph-store, and `DISTILLED_SKILL` helpers
//! previously duplicated across the `lessons`, `registry`, and `render` graph
//! test binaries. Each binary uses only a subset of these helpers, so the module
//! allows dead code rather than warning per binary.

#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use moa_core::types::memory::RlsContext;
use moa_core::{error::MoaError, error::Result, types::action_policy::ActionRuleScope};
use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_memory_graph::PostgresGraphStore;
use moa_memory_types::MemoryScope;
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
