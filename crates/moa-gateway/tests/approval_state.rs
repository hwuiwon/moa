//! Out-of-line tests for Slack gateway approval button lifecycle state.

mod support;

use chrono::{TimeZone, Utc};
use moa_core::{ApprovalDecision, Platform, SessionSignal};
use moa_gateway::{
    ApprovalLifecycleState, ApprovalStateTracker, approval_buttons, approval_state_marker,
    resolved_approval_buttons,
};
use support::{approval_request, fixed_request_id, outbound_text};

#[test]
fn slack_approval_buttons_in_pending_state_are_clickable() {
    let request_id = fixed_request_id();
    let slack_buttons = approval_buttons(Platform::Slack, request_id);

    assert_eq!(slack_buttons.len(), 3);
    assert!(
        slack_buttons
            .iter()
            .all(|button| !button.callback_data.is_empty()),
        "slack pending buttons should be clickable"
    );
}

#[tokio::test]
async fn slack_approval_button_click_with_valid_callback_data_emits_decision_signal() {
    let request = approval_request();
    let request_id = request.request_id;
    let now = fixed_now();
    let tracker = ApprovalStateTracker::new();
    tracker
        .insert_pending(
            request.clone(),
            now + chrono::Duration::minutes(5),
            "@test-user",
        )
        .await;
    let callback_data = approval_buttons(Platform::Slack, request_id)[0]
        .callback_data
        .clone();

    let outcome = tracker
        .handle_callback(&callback_data, "@test-user", now)
        .await;

    assert_eq!(
        outcome.signal,
        Some(SessionSignal::ApprovalDecided {
            request_id,
            decision: ApprovalDecision::AllowOnce,
        })
    );
    assert_eq!(
        outbound_text(&outcome.acknowledgement),
        "Approval recorded."
    );
    assert_eq!(
        outcome.state,
        ApprovalLifecycleState::Decided {
            decision: ApprovalDecision::AllowOnce,
            actor: "@test-user".to_string(),
            decided_at: now,
        }
    );
}

#[tokio::test]
async fn approval_button_click_with_unknown_request_id_returns_stale_error_message() {
    let tracker = ApprovalStateTracker::new();
    let callback_data = approval_buttons(Platform::Slack, fixed_request_id())[0]
        .callback_data
        .clone();

    let outcome = tracker
        .handle_callback(&callback_data, "@test-user", fixed_now())
        .await;

    assert_eq!(outcome.signal, None);
    assert_eq!(
        outbound_text(&outcome.acknowledgement),
        "This approval has expired or already been decided."
    );
    assert!(matches!(
        outcome.state,
        ApprovalLifecycleState::Expired { .. }
    ));
}

#[test]
fn slack_approval_buttons_after_decision_are_removed() {
    let request_id = fixed_request_id();
    let state = ApprovalLifecycleState::Decided {
        decision: ApprovalDecision::AllowOnce,
        actor: "@test-user".to_string(),
        decided_at: fixed_now(),
    };

    assert_eq!(
        approval_state_marker(&state),
        "✓ Allowed by @test-user at 12:34"
    );

    let slack_buttons = resolved_approval_buttons(
        Platform::Slack,
        request_id,
        &ApprovalDecision::AllowOnce,
        "@test-user",
    );
    assert!(
        slack_buttons.is_empty(),
        "Slack should remove approval action buttons after a decision"
    );
}

#[tokio::test]
async fn concurrent_clicks_on_same_approval_request_only_first_wins() {
    for iteration in 0..50 {
        let tracker = ApprovalStateTracker::new();
        let request = approval_request();
        let request_id = request.request_id;
        let now = fixed_now();
        tracker
            .insert_pending(request, now + chrono::Duration::minutes(5), "@first-user")
            .await;
        let allow_callback = format!("ap:o:{request_id}");
        let deny_callback = format!("ap:d:{request_id}");

        let (first, second) = tokio::join!(
            tracker.handle_callback(&allow_callback, "@first-user", now),
            tracker.handle_callback(&deny_callback, "@second-user", now)
        );

        assert_eq!(
            first.signal,
            Some(SessionSignal::ApprovalDecided {
                request_id,
                decision: ApprovalDecision::AllowOnce,
            }),
            "iteration {iteration}: first click should win"
        );
        assert_eq!(
            second.signal, None,
            "iteration {iteration}: second click should not emit a decision"
        );
        assert_eq!(
            outbound_text(&second.acknowledgement),
            "This approval has expired or already been decided.",
            "iteration {iteration}: second click should receive stale-decision text"
        );
        assert_eq!(
            tracker.state(request_id).await,
            Some(ApprovalLifecycleState::Decided {
                decision: ApprovalDecision::AllowOnce,
                actor: "@first-user".to_string(),
                decided_at: now,
            }),
            "iteration {iteration}: tracker should retain the first decision"
        );
    }
}

#[tokio::test]
async fn approval_request_after_orchestrator_timeout_marks_buttons_as_expired() {
    let tracker = ApprovalStateTracker::new();
    let request = approval_request();
    let request_id = request.request_id;
    let now = fixed_now();
    let expires_at = now - chrono::Duration::seconds(1);
    tracker
        .insert_pending(request, expires_at, "@test-user")
        .await;

    let outcome = tracker
        .handle_callback(&format!("ap:o:{request_id}"), "@test-user", now)
        .await;

    assert_eq!(outcome.signal, None);
    assert_eq!(
        outbound_text(&outcome.acknowledgement),
        "This approval has expired."
    );
    assert_eq!(
        outcome.state,
        ApprovalLifecycleState::Expired {
            expired_at: expires_at,
        }
    );
    assert_eq!(approval_state_marker(&outcome.state), "Expired");

    let slack_buttons = resolved_approval_buttons(
        Platform::Slack,
        request_id,
        &ApprovalDecision::Deny {
            reason: Some("expired".to_string()),
        },
        "system",
    );
    assert!(slack_buttons.is_empty());
}

#[tokio::test]
async fn approval_button_click_from_wrong_actor_is_rejected_without_deciding() {
    let tracker = ApprovalStateTracker::new();
    let request = approval_request();
    let request_id = request.request_id;
    let now = fixed_now();
    tracker
        .insert_pending(request, now + chrono::Duration::minutes(5), "@owner")
        .await;

    let outcome = tracker
        .handle_callback(&format!("ap:o:{request_id}"), "@other-user", now)
        .await;

    assert_eq!(outcome.signal, None);
    assert_eq!(
        outbound_text(&outcome.acknowledgement),
        "You are not authorized to decide this approval."
    );
    assert_eq!(
        tracker.state(request_id).await,
        Some(ApprovalLifecycleState::Pending {
            expires_at: now + chrono::Duration::minutes(5),
        }),
        "wrong actor must not mutate approval state"
    );

    let owner_outcome = tracker
        .handle_callback(&format!("ap:o:{request_id}"), "@owner", now)
        .await;
    assert_eq!(
        owner_outcome.signal,
        Some(SessionSignal::ApprovalDecided {
            request_id,
            decision: ApprovalDecision::AllowOnce,
        })
    );
}

fn fixed_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 7, 12, 34, 0)
        .single()
        .expect("fixed timestamp should be valid")
}
