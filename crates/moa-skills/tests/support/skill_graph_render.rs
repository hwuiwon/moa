//! Graph-backed skill-render fixtures.

use moa_core::RlsContext;
use moa_core::{ActionRuleScope, TenantId};
use moa_memory_graph::PostgresGraphStore;
use moa_memory_types::MemoryScope;
use sha2::{Digest, Sha256};
use uuid::Uuid;

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
    PostgresGraphStore::scoped_for_app_role(pool.clone(), RlsContext::from(scope.clone()))
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
