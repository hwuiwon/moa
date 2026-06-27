//! Graph-backed skill-registry fixtures.

use moa_core::{ActionRuleScope, MoaError, Result, TenantId};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) static GRAPH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn tenant_scope(storage_partition_id: &str) -> ActionRuleScope {
    ActionRuleScope::Tenant {
        tenant_id: tenant_id_from_storage_partition(storage_partition_id),
    }
}

fn tenant_id_from_storage_partition(storage_partition_id: &str) -> TenantId {
    if let Ok(uuid) = Uuid::parse_str(storage_partition_id) {
        return TenantId::from(uuid);
    }
    let digest = Sha256::digest(storage_partition_id.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    TenantId::from(Uuid::from_bytes(bytes))
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
