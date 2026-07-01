//! Database enum conversion helpers for session queries.
//!
//! Each enum's persisted label is produced by `strum`-derived `as_str()` (write)
//! and `FromStr` (read) on the `moa_core` definitions, so the `to_db` direction
//! is simply `value.as_str()` at the call site. `from_db` is the single adapter
//! that turns a parse failure into a `StorageError` naming the column and value.

use std::str::FromStr;

use super::*;

/// Parses a stored database label into its enum.
///
/// `kind` names the column for diagnostics; an unrecognized label becomes a
/// [`MoaError::StorageError`] that quotes both the column kind and the value.
pub(crate) fn from_db<E: FromStr>(kind: &str, value: &str) -> Result<E> {
    E::from_str(value)
        .map_err(|_| MoaError::StorageError(format!("unknown {kind} value `{value}`")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::{
        ActionPolicyEffect, ActionRuleScope, AttributionEffect, AttributionSubjectType, Channel,
        EventType, LearningCandidateStatus, LearningCandidateType, LearningRiskClass,
        SegmentOutcome, SessionStatus,
    };

    #[test]
    fn db_labels_are_pinned_and_round_trip() {
        // Pins: the persisted strings strum derives must stay byte-identical to the
        // previous hand-written tables, and every label round-trips via `from_db`.
        let session_statuses = [
            (SessionStatus::Created, "created"),
            (SessionStatus::Running, "running"),
            (SessionStatus::Paused, "paused"),
            (SessionStatus::Completed, "completed"),
            (SessionStatus::Cancelled, "cancelled"),
            (SessionStatus::Failed, "failed"),
        ];
        for (value, label) in session_statuses {
            assert_eq!(value.as_str(), label);
            assert_eq!(
                from_db::<SessionStatus>("session status", label).unwrap(),
                value
            );
        }

        let channels = [(Channel::Slack, "slack"), (Channel::Chat, "chat")];
        for (value, label) in channels {
            assert_eq!(value.as_str(), label);
            assert_eq!(from_db::<Channel>("channel", label).unwrap(), value);
        }

        // Event types persist as verbatim PascalCase (distinct from the snake_case
        // serde/JSON form), so spot-check single- and multi-word variants.
        let event_types = [
            (EventType::SessionCreated, "SessionCreated"),
            (EventType::ToolError, "ToolError"),
            (
                EventType::WorkerNotificationDelivered,
                "WorkerNotificationDelivered",
            ),
            (EventType::GuardrailCheck, "GuardrailCheck"),
            (EventType::Warning, "Warning"),
        ];
        for (value, label) in event_types {
            assert_eq!(value.as_str(), label);
            assert_eq!(from_db::<EventType>("event type", label).unwrap(), value);
        }

        assert_eq!(ActionPolicyEffect::AdminReview.as_str(), "admin_review");
        assert_eq!(
            from_db::<ActionPolicyEffect>("action policy effect", "deny").unwrap(),
            ActionPolicyEffect::Deny
        );
        assert_eq!(
            ActionRuleScope::Tenant {
                tenant_id: moa_core::TenantId::new()
            }
            .as_str(),
            "tenant"
        );

        // Experience / segment enums (formerly hand-rolled in rows.rs).
        let segment_outcomes = [
            (SegmentOutcome::Resolved, "resolved"),
            (SegmentOutcome::Partial, "partial"),
            (SegmentOutcome::Unknown, "unknown"),
            (SegmentOutcome::Failed, "failed"),
            (SegmentOutcome::Abandoned, "abandoned"),
        ];
        for (value, label) in segment_outcomes {
            assert_eq!(value.as_str(), label);
            assert_eq!(
                from_db::<SegmentOutcome>("segment outcome", label).unwrap(),
                value
            );
        }

        let subject_types = [
            (AttributionSubjectType::Skill, "skill"),
            (AttributionSubjectType::Tool, "tool"),
            (AttributionSubjectType::Memory, "memory"),
            (AttributionSubjectType::Policy, "policy"),
            (AttributionSubjectType::Verification, "verification"),
        ];
        for (value, label) in subject_types {
            assert_eq!(value.as_str(), label);
            assert_eq!(
                from_db::<AttributionSubjectType>("attribution subject type", label).unwrap(),
                value
            );
        }

        let effects = [
            (AttributionEffect::Helpful, "helpful"),
            (AttributionEffect::Neutral, "neutral"),
            (AttributionEffect::Harmful, "harmful"),
            (AttributionEffect::Mixed, "mixed"),
        ];
        for (value, label) in effects {
            assert_eq!(value.as_str(), label);
            assert_eq!(
                from_db::<AttributionEffect>("attribution effect", label).unwrap(),
                value
            );
        }

        let candidate_types = [
            (LearningCandidateType::Skill, "skill"),
            (LearningCandidateType::Memory, "memory"),
            (LearningCandidateType::Policy, "policy"),
            (LearningCandidateType::Eval, "eval"),
            (LearningCandidateType::Prompt, "prompt"),
            (LearningCandidateType::Workflow, "workflow"),
        ];
        for (value, label) in candidate_types {
            assert_eq!(value.as_str(), label);
            assert_eq!(
                from_db::<LearningCandidateType>("learning candidate type", label).unwrap(),
                value
            );
        }

        let candidate_statuses = [
            (LearningCandidateStatus::Proposed, "proposed"),
            (LearningCandidateStatus::Evaluating, "evaluating"),
            (LearningCandidateStatus::Promoted, "promoted"),
            (LearningCandidateStatus::Rejected, "rejected"),
            (LearningCandidateStatus::RolledBack, "rolled_back"),
        ];
        for (value, label) in candidate_statuses {
            assert_eq!(value.as_str(), label);
            assert_eq!(
                from_db::<LearningCandidateStatus>("learning candidate status", label).unwrap(),
                value
            );
        }

        let risk_classes = [
            (LearningRiskClass::Low, "low"),
            (LearningRiskClass::Medium, "medium"),
            (LearningRiskClass::High, "high"),
        ];
        for (value, label) in risk_classes {
            assert_eq!(value.as_str(), label);
            assert_eq!(
                from_db::<LearningRiskClass>("learning risk class", label).unwrap(),
                value
            );
        }
    }

    #[test]
    fn unknown_labels_error_with_the_offending_value() {
        let error = from_db::<SessionStatus>("session status", "bogus").unwrap_err();
        assert!(error.to_string().contains("bogus"));
        // PascalCase event types must not accept the snake_case serde form.
        assert!(from_db::<EventType>("event type", "session_created").is_err());
    }
}
