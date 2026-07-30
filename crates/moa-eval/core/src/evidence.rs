//! Versioned run evidence captured before teardown.
//!
//! An [`EvidenceEnvelope`] is the only thing a typed assertion is allowed to
//! read. It is captured while the run's environment still exists — final world
//! state, the ordered action/approval ledger, conversation history, and lineage
//! references — so an assertion never has to re-derive a fact from a torn-down
//! sandbox or from a lossy free-text summary.
//!
//! The envelope is deliberately hostile to optimistic reading:
//! [`EvidenceEnvelope::validate`] rejects a wrong schema version, a capture the
//! producer flagged as truncated or missing, a declared/observed count
//! mismatch, and duplicate or out-of-order ledger sequences. Assertion
//! evaluation treats every one of those as a hard failure rather than as an
//! absent observation, so a run that lost its evidence can never certify
//! itself.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema version of [`EvidenceEnvelope`].
///
/// Bumping this invalidates every persisted envelope on purpose: there is no
/// compatibility deserializer, and an envelope from another version fails
/// closed.
pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;

/// Identity of the `(case, config)` run an envelope describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvidenceSubject {
    /// Test case name the evidence was captured for.
    pub case: String,
    /// Case-model schema version the evidence was captured under.
    pub case_schema_version: u32,
    /// Agent configuration name the case ran against.
    pub agent_config: String,
    /// Free-form label distinguishing repeated runs of the same subject.
    pub run_label: String,
}

/// What one ordered ledger entry represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// The agent invoked a tool or effectful action.
    Invocation,
    /// An approval was requested for a named action.
    ApprovalRequested,
    /// An approval was granted for a named action.
    ApprovalGranted,
    /// An approval was denied for a named action.
    ApprovalDenied,
}

/// Terminal outcome of one ordered ledger entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    /// The action ran and reported success.
    Succeeded,
    /// The action ran and reported failure.
    Failed,
    /// The action was refused before it could take effect.
    Rejected,
    /// The entry records a fact rather than an effect (approvals).
    Recorded,
}

/// One entry in the ordered action and approval ledger.
///
/// `sequence` is the run-global ordering key. Ordering and approval assertions
/// depend on it being strictly increasing, which is exactly why
/// [`EvidenceEnvelope::validate`] rejects duplicates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionRecord {
    /// Strictly increasing run-global ordering key.
    pub sequence: u64,
    /// What this entry represents.
    pub kind: ActionKind,
    /// Action or approval subject name.
    pub name: String,
    /// Structured action arguments.
    #[serde(default)]
    pub arguments: Value,
    /// Terminal outcome of the entry.
    pub outcome: ActionOutcome,
}

/// Speaker role for one conversation history record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRole {
    /// End-user turn.
    User,
    /// Agent turn.
    Assistant,
    /// Tool output rendered into history.
    Tool,
    /// System or harness directive.
    System,
}

/// One conversation history record available to semantic assertions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecord {
    /// Strictly increasing run-global ordering key.
    pub sequence: u64,
    /// Speaker role.
    pub role: HistoryRole,
    /// Recorded text.
    pub text: String,
}

/// One lineage or citation fact recorded during the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageRecord {
    /// Strictly increasing run-global ordering key.
    pub sequence: u64,
    /// Lineage category, such as `memory_read` or `citation`.
    pub kind: String,
    /// Stable reference the run consumed or produced.
    pub reference: String,
}

/// Everything an assertion may read about a completed run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvidenceObservations {
    /// Final observable world state, keyed by a stable domain path.
    ///
    /// Harnesses without an environment oracle leave this empty, which makes
    /// any environment-state assertion fail rather than pass vacuously.
    pub final_state: BTreeMap<String, Value>,
    /// Ordered action and approval ledger.
    pub actions: Vec<ActionRecord>,
    /// Ordered conversation history.
    pub history: Vec<HistoryRecord>,
    /// Ordered lineage and citation references.
    pub lineage: Vec<LineageRecord>,
    /// Final agent response text, when one was produced and captured.
    pub response: Option<String>,
}

/// Producer-declared completeness of a capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CaptureCompleteness {
    /// Every observation the producer intended to capture is present.
    #[default]
    Complete,
    /// The producer captured a partial view.
    Truncated {
        /// Why the capture is partial.
        reason: String,
    },
    /// The producer could not capture observations at all.
    Missing {
        /// Why the capture is absent.
        reason: String,
    },
}

/// Where an envelope came from and how complete it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceProvenance {
    /// Capture timestamp, taken before teardown.
    pub captured_at: DateTime<Utc>,
    /// Stable producer identity, such as `session_event_log` or `mock_domain`.
    pub source: String,
    /// Producer-declared completeness.
    pub capture: CaptureCompleteness,
    /// Number of action records the producer intended to emit.
    pub declared_action_count: u64,
    /// Number of history records the producer intended to emit.
    pub declared_history_count: u64,
}

impl Default for EvidenceProvenance {
    fn default() -> Self {
        Self {
            captured_at: Utc::now(),
            source: String::new(),
            capture: CaptureCompleteness::Complete,
            declared_action_count: 0,
            declared_history_count: 0,
        }
    }
}

/// Versioned evidence for one evaluation run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEnvelope {
    /// Envelope schema version; must equal [`EVIDENCE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Run identity the observations belong to.
    pub subject: EvidenceSubject,
    /// Observations captured before teardown.
    pub observations: EvidenceObservations,
    /// Capture provenance and completeness.
    pub provenance: EvidenceProvenance,
}

impl Default for EvidenceEnvelope {
    fn default() -> Self {
        Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            subject: EvidenceSubject::default(),
            observations: EvidenceObservations::default(),
            provenance: EvidenceProvenance::default(),
        }
    }
}

/// A reason an envelope may not be trusted.
///
/// This is intentionally not an error type: a defect is an assertion *result*,
/// not a control-flow failure, and every defect makes blocking assertions fail
/// closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceDefect {
    /// No envelope was captured for the run.
    Missing {
        /// Why the envelope is absent.
        reason: String,
    },
    /// The envelope was produced under a different schema version.
    WrongSchemaVersion {
        /// Version this build accepts.
        expected: u32,
        /// Version the envelope declared.
        found: u32,
    },
    /// The producer captured only part of the run.
    Truncated {
        /// Why the capture is partial.
        reason: String,
    },
    /// The envelope repeats an ordering key.
    Duplicate {
        /// Which ledger and key repeated.
        detail: String,
    },
    /// The envelope contradicts itself.
    Inconsistent {
        /// What contradicts what.
        detail: String,
    },
}

impl fmt::Display for EvidenceDefect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { reason } => write!(formatter, "evidence is missing: {reason}"),
            Self::WrongSchemaVersion { expected, found } => write!(
                formatter,
                "evidence schema version {found} is not the required version {expected}"
            ),
            Self::Truncated { reason } => write!(formatter, "evidence is truncated: {reason}"),
            Self::Duplicate { detail } => write!(formatter, "evidence is duplicated: {detail}"),
            Self::Inconsistent { detail } => {
                write!(formatter, "evidence is inconsistent: {detail}")
            }
        }
    }
}

impl EvidenceEnvelope {
    /// Starts a builder that assigns ledger sequences monotonically.
    #[must_use]
    pub fn builder(subject: EvidenceSubject) -> EvidenceBuilder {
        EvidenceBuilder::new(subject)
    }

    /// Returns the first defect that makes this envelope untrustworthy.
    ///
    /// Checks run in fail-closed order: version, producer-declared
    /// completeness, declared-versus-observed counts, then ledger ordering.
    pub fn validate(&self) -> std::result::Result<(), EvidenceDefect> {
        if self.schema_version != EVIDENCE_SCHEMA_VERSION {
            return Err(EvidenceDefect::WrongSchemaVersion {
                expected: EVIDENCE_SCHEMA_VERSION,
                found: self.schema_version,
            });
        }

        if self.provenance.source.trim().is_empty() {
            return Err(EvidenceDefect::Inconsistent {
                detail: "provenance.source is empty".to_string(),
            });
        }

        match &self.provenance.capture {
            CaptureCompleteness::Complete => {}
            CaptureCompleteness::Truncated { reason } => {
                return Err(EvidenceDefect::Truncated {
                    reason: reason.clone(),
                });
            }
            CaptureCompleteness::Missing { reason } => {
                return Err(EvidenceDefect::Missing {
                    reason: reason.clone(),
                });
            }
        }

        let observed_actions = self.observations.actions.len() as u64;
        if self.provenance.declared_action_count != observed_actions {
            return Err(EvidenceDefect::Truncated {
                reason: format!(
                    "declared {} action records but captured {observed_actions}",
                    self.provenance.declared_action_count
                ),
            });
        }

        let observed_history = self.observations.history.len() as u64;
        if self.provenance.declared_history_count != observed_history {
            return Err(EvidenceDefect::Truncated {
                reason: format!(
                    "declared {} history records but captured {observed_history}",
                    self.provenance.declared_history_count
                ),
            });
        }

        check_sequences(
            "actions",
            self.observations
                .actions
                .iter()
                .map(|record| record.sequence),
        )?;
        check_sequences(
            "history",
            self.observations
                .history
                .iter()
                .map(|record| record.sequence),
        )?;
        check_sequences(
            "lineage",
            self.observations
                .lineage
                .iter()
                .map(|record| record.sequence),
        )?;

        Ok(())
    }

    /// Returns every invocation entry with the given action name.
    pub fn invocations<'envelope>(
        &'envelope self,
        name: &'envelope str,
    ) -> impl Iterator<Item = &'envelope ActionRecord> {
        self.observations
            .actions
            .iter()
            .filter(move |record| record.kind == ActionKind::Invocation && record.name == name)
    }

    /// Returns the ordered invocation names observed during the run.
    #[must_use]
    pub fn invocation_names(&self) -> Vec<&str> {
        self.observations
            .actions
            .iter()
            .filter(|record| record.kind == ActionKind::Invocation)
            .map(|record| record.name.as_str())
            .collect()
    }
}

fn check_sequences(
    ledger: &str,
    sequences: impl Iterator<Item = u64>,
) -> std::result::Result<(), EvidenceDefect> {
    let mut previous: Option<u64> = None;
    for sequence in sequences {
        if let Some(previous) = previous {
            if sequence == previous {
                return Err(EvidenceDefect::Duplicate {
                    detail: format!("{ledger} ledger repeats sequence {sequence}"),
                });
            }
            if sequence < previous {
                return Err(EvidenceDefect::Inconsistent {
                    detail: format!(
                        "{ledger} ledger sequence {sequence} follows {previous} out of order"
                    ),
                });
            }
        }
        previous = Some(sequence);
    }
    Ok(())
}

/// Builds an envelope with monotonically assigned ledger sequences.
///
/// The builder is the supported producer path precisely because it cannot emit
/// a duplicate or out-of-order sequence, and it keeps the declared counts in
/// step with what was actually appended.
#[derive(Debug, Clone)]
pub struct EvidenceBuilder {
    subject: EvidenceSubject,
    observations: EvidenceObservations,
    source: String,
    capture: CaptureCompleteness,
    next_sequence: u64,
}

impl EvidenceBuilder {
    /// Creates a builder for one run subject.
    #[must_use]
    pub fn new(subject: EvidenceSubject) -> Self {
        Self {
            subject,
            observations: EvidenceObservations::default(),
            source: String::new(),
            capture: CaptureCompleteness::Complete,
            next_sequence: 1,
        }
    }

    /// Sets the producer identity recorded in provenance.
    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Marks the capture as partial, which makes the envelope fail closed.
    #[must_use]
    pub fn truncated(mut self, reason: impl Into<String>) -> Self {
        self.capture = CaptureCompleteness::Truncated {
            reason: reason.into(),
        };
        self
    }

    /// Records one final-state fact.
    #[must_use]
    pub fn state(mut self, key: impl Into<String>, value: Value) -> Self {
        self.observations.final_state.insert(key.into(), value);
        self
    }

    /// Appends one ledger entry with the next sequence.
    #[must_use]
    pub fn action(
        mut self,
        kind: ActionKind,
        name: impl Into<String>,
        arguments: Value,
        outcome: ActionOutcome,
    ) -> Self {
        let sequence = self.take_sequence();
        self.observations.actions.push(ActionRecord {
            sequence,
            kind,
            name: name.into(),
            arguments,
            outcome,
        });
        self
    }

    /// Appends one history record with the next sequence.
    #[must_use]
    pub fn history(mut self, role: HistoryRole, text: impl Into<String>) -> Self {
        let sequence = self.take_sequence();
        self.observations.history.push(HistoryRecord {
            sequence,
            role,
            text: text.into(),
        });
        self
    }

    /// Appends one lineage record with the next sequence.
    #[must_use]
    pub fn lineage(mut self, kind: impl Into<String>, reference: impl Into<String>) -> Self {
        let sequence = self.take_sequence();
        self.observations.lineage.push(LineageRecord {
            sequence,
            kind: kind.into(),
            reference: reference.into(),
        });
        self
    }

    /// Sets the final response text.
    #[must_use]
    pub fn response(mut self, response: impl Into<String>) -> Self {
        self.observations.response = Some(response.into());
        self
    }

    /// Finishes the envelope, deriving declared counts from what was appended.
    #[must_use]
    pub fn build(self) -> EvidenceEnvelope {
        let declared_action_count = self.observations.actions.len() as u64;
        let declared_history_count = self.observations.history.len() as u64;
        EvidenceEnvelope {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            subject: self.subject,
            observations: self.observations,
            provenance: EvidenceProvenance {
                captured_at: Utc::now(),
                source: self.source,
                capture: self.capture,
                declared_action_count,
                declared_history_count,
            },
        }
    }

    fn take_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        sequence
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActionKind, ActionOutcome, ActionRecord, CaptureCompleteness, EVIDENCE_SCHEMA_VERSION,
        EvidenceBuilder, EvidenceDefect, EvidenceEnvelope, EvidenceSubject, HistoryRole,
    };
    use serde_json::json;

    fn builder() -> EvidenceBuilder {
        EvidenceBuilder::new(EvidenceSubject {
            case: "case".to_string(),
            case_schema_version: 2,
            agent_config: "config".to_string(),
            run_label: "run".to_string(),
        })
        .source("unit_test")
    }

    #[test]
    fn a_builder_envelope_validates() {
        let envelope = builder()
            .action(
                ActionKind::Invocation,
                "deploy",
                json!({ "env": "staging" }),
                ActionOutcome::Succeeded,
            )
            .response("done")
            .build();

        assert_eq!(envelope.validate(), Ok(()));
        assert_eq!(envelope.invocation_names(), vec!["deploy"]);
    }

    #[test]
    fn a_wrong_schema_version_is_a_defect() {
        let mut envelope = builder().build();
        envelope.schema_version = EVIDENCE_SCHEMA_VERSION + 1;

        assert_eq!(
            envelope.validate(),
            Err(EvidenceDefect::WrongSchemaVersion {
                expected: EVIDENCE_SCHEMA_VERSION,
                found: EVIDENCE_SCHEMA_VERSION + 1,
            })
        );
    }

    #[test]
    fn a_truncated_capture_is_a_defect() {
        let envelope = builder().truncated("content capture disabled").build();

        assert!(matches!(
            envelope.validate(),
            Err(EvidenceDefect::Truncated { .. })
        ));
    }

    #[test]
    fn a_declared_count_mismatch_reads_as_truncation() {
        // Pins: dropping a record without updating the declared count is the
        // realistic truncation shape, and it must not read as "no actions".
        let mut envelope = builder()
            .action(
                ActionKind::Invocation,
                "deploy",
                json!({}),
                ActionOutcome::Succeeded,
            )
            .build();
        envelope.observations.actions.clear();

        assert!(matches!(
            envelope.validate(),
            Err(EvidenceDefect::Truncated { .. })
        ));
    }

    #[test]
    fn a_repeated_sequence_is_a_duplicate_defect() {
        let mut envelope = builder()
            .action(
                ActionKind::Invocation,
                "deploy",
                json!({}),
                ActionOutcome::Succeeded,
            )
            .build();
        envelope.observations.actions.push(ActionRecord {
            sequence: 1,
            kind: ActionKind::Invocation,
            name: "deploy".to_string(),
            arguments: json!({}),
            outcome: ActionOutcome::Succeeded,
        });
        envelope.provenance.declared_action_count = 2;

        assert!(matches!(
            envelope.validate(),
            Err(EvidenceDefect::Duplicate { .. })
        ));
    }

    #[test]
    fn an_empty_source_is_inconsistent() {
        let mut envelope = builder().build();
        envelope.provenance.source = String::new();

        assert!(matches!(
            envelope.validate(),
            Err(EvidenceDefect::Inconsistent { .. })
        ));
    }

    #[test]
    fn an_envelope_round_trips_through_json() {
        let envelope = builder()
            .state("deploy.production", json!("2.1"))
            .action(
                ActionKind::ApprovalGranted,
                "deploy",
                json!({}),
                ActionOutcome::Recorded,
            )
            .history(HistoryRole::User, "ship it")
            .lineage("citation", "TCK-1")
            .response("shipped")
            .build();

        let encoded = serde_json::to_string(&envelope).expect("serialize");
        let decoded: EvidenceEnvelope = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, envelope);
        assert_eq!(decoded.validate(), Ok(()));
        assert_eq!(
            decoded.provenance.capture,
            CaptureCompleteness::Complete,
            "a complete capture must survive the round trip"
        );
    }
}
