//! Out-of-line tests for gateway slash-command control-signal translation.

mod support;

use moa_core::{Platform, SessionSignal};
use moa_gateway::control_action_for_inbound;
use support::{fixed_session_id, inbound_message, outbound_text};

#[test]
fn telegram_slash_stop_command_emits_softcancel_signal_with_acknowledgement() {
    let inbound = inbound_message(Platform::Telegram, "/stop");
    let action =
        control_action_for_inbound(Platform::Telegram, &fixed_session_id(), &inbound, true)
            .expect("telegram /stop should produce a control action");

    assert_eq!(action.signal, Some(SessionSignal::SoftCancel));
    assert_eq!(outbound_text(&action.acknowledgement), "Stopping...");
    assert!(!action.acknowledgement.ephemeral);
}

#[test]
fn slack_slash_stop_command_emits_softcancel_signal_with_ephemeral_ack() {
    let inbound = inbound_message(Platform::Slack, "/stop");
    let action = control_action_for_inbound(Platform::Slack, &fixed_session_id(), &inbound, true)
        .expect("slack /stop should produce a control action");

    assert_eq!(action.signal, Some(SessionSignal::SoftCancel));
    assert_eq!(outbound_text(&action.acknowledgement), "Stopping...");
    assert!(action.acknowledgement.ephemeral);
}

#[test]
fn discord_slash_stop_command_emits_softcancel_signal_with_followup_ack() {
    let inbound = inbound_message(Platform::Discord, "/stop");
    let action = control_action_for_inbound(Platform::Discord, &fixed_session_id(), &inbound, true)
        .expect("discord /stop should produce a control action");

    assert_eq!(action.signal, Some(SessionSignal::SoftCancel));
    assert_eq!(outbound_text(&action.acknowledgement), "Stopping...");
    assert!(!action.acknowledgement.ephemeral);
}

#[test]
fn slash_stop_with_force_flag_emits_hardcancel_signal_instead() {
    for text in ["/stop --force", "/stop force"] {
        let inbound = inbound_message(Platform::Telegram, text);
        let action =
            control_action_for_inbound(Platform::Telegram, &fixed_session_id(), &inbound, true)
                .expect("force stop should produce a control action");

        assert_eq!(action.signal, Some(SessionSignal::HardCancel));
        assert_eq!(
            outbound_text(&action.acknowledgement),
            "Stopping immediately..."
        );
    }
}

#[test]
fn message_arriving_during_active_session_emits_queuemessage_signal() {
    let inbound = inbound_message(Platform::Slack, "please also run cargo test");
    let action = control_action_for_inbound(Platform::Slack, &fixed_session_id(), &inbound, true)
        .expect("message during active session should queue");

    match action.signal {
        Some(SessionSignal::QueueMessage(message)) => {
            assert_eq!(message.text, "please also run cargo test");
            assert!(message.attachments.is_empty());
        }
        other => panic!("expected QueueMessage signal, got {other:?}"),
    }
    assert_eq!(
        outbound_text(&action.acknowledgement),
        "Queued - will be picked up after current task"
    );
}

#[test]
fn slash_command_with_unknown_verb_returns_help_text_and_does_not_emit_signal() {
    let inbound = inbound_message(Platform::Discord, "/wat");
    let action = control_action_for_inbound(Platform::Discord, &fixed_session_id(), &inbound, true)
        .expect("unknown slash command should produce a help acknowledgement");

    assert_eq!(action.signal, None);
    let ack = outbound_text(&action.acknowledgement);
    assert!(ack.contains("Unknown command"));
    assert!(ack.contains("/stop"));
    assert!(ack.contains("/queue"));
}
