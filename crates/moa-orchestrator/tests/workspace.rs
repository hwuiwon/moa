//! Unit coverage for workspace scheduling helpers.

use chrono::{TimeZone, Utc};
use moa_core::WorkspaceId;
use moa_orchestrator::objects::workspace::{
    compute_next_consolidation_utc, deterministic_consolidation_jitter_secs,
};

#[test]
fn compute_next_consolidation_same_day_when_hour_is_still_ahead() {
    let now = Utc
        .with_ymd_and_hms(2026, 4, 20, 1, 15, 0)
        .single()
        .expect("valid timestamp");

    let next = compute_next_consolidation_utc(now, 3);

    assert_eq!(
        next,
        Utc.with_ymd_and_hms(2026, 4, 20, 3, 0, 0)
            .single()
            .expect("valid timestamp")
    );
}

#[test]
fn compute_next_consolidation_rolls_over_after_target_hour() {
    let now = Utc
        .with_ymd_and_hms(2026, 4, 20, 23, 15, 0)
        .single()
        .expect("valid timestamp");

    let next = compute_next_consolidation_utc(now, 0);

    assert_eq!(
        next,
        Utc.with_ymd_and_hms(2026, 4, 21, 0, 0, 0)
            .single()
            .expect("valid timestamp")
    );
}

#[test]
fn consolidation_jitter_is_deterministic_per_workspace() {
    let workspace_id = WorkspaceId::new("workspace-r09");

    let first = deterministic_consolidation_jitter_secs(&workspace_id);
    let second = deterministic_consolidation_jitter_secs(&workspace_id);

    assert_eq!(first, second);
    assert!(first < 600);
}
