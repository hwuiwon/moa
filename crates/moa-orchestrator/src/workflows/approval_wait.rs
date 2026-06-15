//! Shared approval-wait policy helpers for Restate workflows.

use std::time::Duration;

use moa_core::ApprovalDecision;

const APPROVAL_TIMEOUT_SECS_ENV: &str = "MOA_APPROVAL_TIMEOUT_SECS";
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 30 * 60;
const CANCELLED_APPROVAL_PREFIX: &str = "Cancelled while waiting for approval:";

/// Parses an approval wait timeout from an optional environment value.
pub(crate) fn timeout_from_env(raw: Option<&str>, default_secs: u64) -> Duration {
    raw.and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

/// Returns the configured approval wait timeout for workflow approval gates.
pub(crate) fn configured_timeout() -> Duration {
    timeout_from_env(
        std::env::var(APPROVAL_TIMEOUT_SECS_ENV).ok().as_deref(),
        DEFAULT_APPROVAL_TIMEOUT_SECS,
    )
}

/// Builds the deterministic auto-deny reason for an approval wait timeout.
pub(crate) fn timeout_reason(timeout: Duration) -> String {
    format!(
        "Auto-denied: no decision within {} minutes",
        timeout.as_secs() / 60
    )
}

/// Builds the deterministic deny reason for cancellation during approval wait.
pub(crate) fn cancel_reason(reason: &str) -> String {
    format!("{CANCELLED_APPROVAL_PREFIX} {reason}")
}

/// Returns whether an approval denial reason came from cancellation.
pub(crate) fn is_cancel_reason(reason: &str) -> bool {
    reason.starts_with(CANCELLED_APPROVAL_PREFIX)
}

/// Returns the metric label for an approval decision.
pub(crate) fn outcome_label<'a>(
    decision: &'a ApprovalDecision,
    timed_out_reason: &'a str,
) -> &'a str {
    match decision {
        ApprovalDecision::AllowOnce => "allow_once",
        ApprovalDecision::AlwaysAllow { .. } => "always_allow",
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if reason == timed_out_reason => "timeout",
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if is_cancel_reason(reason) => "cancel",
        ApprovalDecision::Deny { .. } => "deny",
    }
}

/// Returns the durable actor id that should be recorded for system decisions.
pub(crate) fn system_decider_for<'a>(
    decision: &'a ApprovalDecision,
    timed_out_reason: &'a str,
) -> Option<&'a str> {
    match decision {
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if reason == timed_out_reason => Some("system:auto-timeout"),
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if is_cancel_reason(reason) => Some("system:cancel"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_defaults_when_override_is_missing_or_invalid() {
        // Pins: approval gates cannot disable timeout with bad env overrides.
        assert_eq!(timeout_from_env(None, 1800), Duration::from_secs(1800));
        assert_eq!(
            timeout_from_env(Some("not-a-number"), 1800),
            Duration::from_secs(1800)
        );
        assert_eq!(timeout_from_env(Some("0"), 1800), Duration::from_secs(1800));
        assert_eq!(timeout_from_env(Some("45"), 1800), Duration::from_secs(45));
    }

    #[test]
    fn outcome_labels_distinguish_cancel_from_timeout() {
        // Pins: approval wait metrics classify cancellation separately from denial and timeout.
        let timed_out_reason = "Auto-denied: no decision within 30 minutes";
        assert_eq!(
            outcome_label(
                &ApprovalDecision::Deny {
                    reason: Some(timed_out_reason.to_string())
                },
                timed_out_reason
            ),
            "timeout"
        );
        assert_eq!(
            outcome_label(
                &ApprovalDecision::Deny {
                    reason: Some(cancel_reason("stop"))
                },
                timed_out_reason
            ),
            "cancel"
        );
    }

    #[test]
    fn system_decider_tracks_timeout_and_cancel() {
        // Pins: ApprovalDecided events attribute workflow-owned decisions to system actors.
        let timed_out_reason = "Auto-denied: no decision within 30 minutes";
        assert_eq!(
            system_decider_for(
                &ApprovalDecision::Deny {
                    reason: Some(timed_out_reason.to_string())
                },
                timed_out_reason
            ),
            Some("system:auto-timeout")
        );
        assert_eq!(
            system_decider_for(
                &ApprovalDecision::Deny {
                    reason: Some(cancel_reason("stop"))
                },
                timed_out_reason
            ),
            Some("system:cancel")
        );
        assert_eq!(
            system_decider_for(&ApprovalDecision::AllowOnce, timed_out_reason),
            None
        );
    }
}
