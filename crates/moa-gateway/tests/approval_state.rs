//! Out-of-line tests for gateway approval button lifecycle state.

mod support;

use chrono::{TimeZone, Utc};
use moa_core::{ApprovalDecision, Platform, SessionSignal};
use moa_gateway::{
    ApprovalLifecycleState, ApprovalStateTracker, approval_buttons, approval_state_marker, discord,
    resolved_approval_buttons, telegram,
};
use support::{approval_request, fixed_request_id, outbound_text};

#[test]
fn approval_buttons_in_pending_state_render_as_clickable_per_platform() {
    let request_id = fixed_request_id();

    let telegram_buttons = approval_buttons(Platform::Telegram, request_id);
    let telegram_markup =
        telegram::render_inline_keyboard(&telegram_buttons).expect("telegram buttons render");
    let telegram_value =
        serde_json::to_value(telegram_markup).expect("telegram markup should serialize");
    for button in telegram_value["inline_keyboard"][0]
        .as_array()
        .expect("telegram markup should contain one button row")
    {
        assert!(
            button["callback_data"]
                .as_str()
                .expect("telegram button callback should be set")
                .starts_with("ap:"),
            "telegram pending button should be clickable"
        );
    }

    let slack_buttons = approval_buttons(Platform::Slack, request_id);
    assert_eq!(slack_buttons.len(), 3);
    assert!(
        slack_buttons
            .iter()
            .all(|button| !button.callback_data.is_empty()),
        "slack pending buttons should be clickable"
    );

    let discord_buttons = approval_buttons(Platform::Discord, request_id);
    let discord_rows = discord::render_action_rows(&discord_buttons, false);
    let discord_value = serde_json::to_value(discord_rows).expect("discord rows should serialize");
    for component in discord_value[0]["components"]
        .as_array()
        .expect("discord row should contain button components")
    {
        assert_eq!(component["disabled"], false);
        assert!(
            component["custom_id"]
                .as_str()
                .expect("discord custom_id should be set")
                .starts_with("ap:"),
            "discord pending button should be clickable"
        );
    }
}

#[tokio::test]
async fn approval_button_click_with_valid_callback_data_emits_decision_signal() {
    let request = approval_request();
    let request_id = request.request_id;
    let now = fixed_now();

    for platform in [Platform::Telegram, Platform::Slack, Platform::Discord] {
        let tracker = ApprovalStateTracker::new();
        tracker
            .insert_pending(request.clone(), now + chrono::Duration::minutes(5))
            .await;
        let callback_data = approval_buttons(platform.clone(), request_id)[0]
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
            }),
            "{platform} callback should emit the expected approval decision"
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
}

#[tokio::test]
async fn approval_button_click_with_unknown_request_id_returns_stale_error_message() {
    let tracker = ApprovalStateTracker::new();
    let callback_data = approval_buttons(Platform::Telegram, fixed_request_id())[0]
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
fn approval_buttons_after_decision_re_render_as_disabled_with_decision_marker() {
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

    let telegram_buttons = resolved_approval_buttons(
        Platform::Telegram,
        request_id,
        &ApprovalDecision::AllowOnce,
        "@test-user",
    );
    let telegram_markup =
        telegram::render_inline_keyboard(&telegram_buttons).expect("telegram marker renders");
    let telegram_value =
        serde_json::to_value(telegram_markup).expect("telegram marker should serialize");
    assert_eq!(
        telegram_value["inline_keyboard"][0][0]["text"],
        "✓ Allowed by @test-user"
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

    let discord_buttons = resolved_approval_buttons(
        Platform::Discord,
        request_id,
        &ApprovalDecision::AllowOnce,
        "@test-user",
    );
    let discord_rows = discord::render_action_rows(&discord_buttons, true);
    let discord_value =
        serde_json::to_value(discord_rows).expect("discord disabled rows should serialize");
    for component in discord_value[0]["components"]
        .as_array()
        .expect("discord row should contain disabled buttons")
    {
        assert_eq!(component["disabled"], true);
    }
}

#[tokio::test]
async fn concurrent_clicks_on_same_approval_request_only_first_wins() {
    for iteration in 0..50 {
        let tracker = ApprovalStateTracker::new();
        let request = approval_request();
        let request_id = request.request_id;
        let now = fixed_now();
        tracker
            .insert_pending(request, now + chrono::Duration::minutes(5))
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
    tracker.insert_pending(request, expires_at).await;

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

    let telegram_buttons = resolved_approval_buttons(
        Platform::Telegram,
        request_id,
        &ApprovalDecision::Deny {
            reason: Some("expired".to_string()),
        },
        "system",
    );
    assert_eq!(telegram_buttons.len(), 1);

    let slack_buttons = resolved_approval_buttons(
        Platform::Slack,
        request_id,
        &ApprovalDecision::Deny {
            reason: Some("expired".to_string()),
        },
        "system",
    );
    assert!(slack_buttons.is_empty());

    let discord_buttons = resolved_approval_buttons(
        Platform::Discord,
        request_id,
        &ApprovalDecision::Deny {
            reason: Some("expired".to_string()),
        },
        "system",
    );
    let discord_rows = discord::render_action_rows(&discord_buttons, true);
    let discord_value =
        serde_json::to_value(discord_rows).expect("discord expired rows should serialize");
    for component in discord_value[0]["components"]
        .as_array()
        .expect("discord expired row should contain disabled buttons")
    {
        assert_eq!(component["disabled"], true);
    }
}

fn fixed_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 7, 12, 34, 0)
        .single()
        .expect("fixed timestamp should be valid")
}
