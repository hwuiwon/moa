//! Cold-tier partition key coverage.

use chrono::{TimeZone, Utc};
use moa_lineage_cold::partition_key;

#[test]
fn partition_key_uses_workspace_and_day_layout() {
    let ts = Utc
        .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
        .single()
        .expect("test timestamp should be valid");

    let key = partition_key("v1", true, "workspace-a", ts);

    assert!(key.starts_with("v1/workspace_id=workspace-a/dt=2026-04-30/"));
    assert!(key.ends_with(".parquet"));
}
