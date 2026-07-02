//! Offline lineage-audit tests, consolidated into one harness binary.

// Shared fixture loaders, declared once so the same support file is not
// loaded as a module multiple times across the merged test modules.
#[path = "support/fixture_json.rs"]
mod fixture_json;
#[path = "support/fixture_jsonl.rs"]
mod fixture_jsonl;

#[path = "lineage_audit_offline/dsar.rs"]
mod dsar;
#[path = "lineage_audit_offline/hash_chain.rs"]
mod hash_chain;
#[path = "lineage_audit_offline/merkle.rs"]
mod merkle;
#[path = "lineage_audit_offline/signing.rs"]
mod signing;
