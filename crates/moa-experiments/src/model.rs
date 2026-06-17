//! Typed experiment definitions and run records.

use chrono::{DateTime, Utc};
use moa_core::{Attachment, MemoryScope, ModelId, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Lifecycle state for an experiment run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentRunStatus {
    /// The run has been accepted but has not started execution.
    Accepted,
    /// The run is executing.
    Running,
    /// The run is blocked on an approval decision.
    WaitingApproval,
    /// The run finished successfully.
    Completed,
    /// The run finished with an error.
    Failed,
    /// The run was cancelled before completion.
    Cancelled,
}

impl ExperimentRunStatus {
    /// Returns the persisted database representation for this status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses a status loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "accepted" => Some(Self::Accepted),
            "running" => Some(Self::Running),
            "waiting_approval" => Some(Self::WaitingApproval),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Execution shape targeted by an experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentTargetKind {
    /// An open-ended agent loop backed by a session turn.
    AgentLoop,
    /// An artifact-backed workflow run.
    Workflow,
}

impl ExperimentTargetKind {
    /// Returns the persisted database representation for this target kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
            Self::Workflow => "workflow",
        }
    }

    /// Parses a target kind loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "agent_loop" => Some(Self::AgentLoop),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }
}

/// Product intent for an experiment run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentRunKind {
    /// Offline or replay-style regression evaluation.
    RegressionEval,
    /// Live behavior experiment against production execution paths.
    LiveBehaviorExperiment,
}

/// Target payload for an experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExperimentTarget {
    /// Run an agent loop prompt through the session execution path.
    AgentLoop {
        /// User-facing prompt used to start or continue the agent loop.
        prompt: String,
        /// Existing session to continue, or `None` for a new session.
        session_id: Option<SessionId>,
        /// Model requested for the agent loop.
        model: ModelId,
        /// Input attachments supplied with the prompt.
        attachments: Vec<Attachment>,
    },
    /// Run an artifact-backed workflow.
    Workflow {
        /// Stable workflow artifact reference such as `workflow://name`.
        workflow_ref: String,
        /// Workflow input payload.
        input: Value,
        /// Optional session linked to workflow history.
        session_id: Option<SessionId>,
        /// Optional idempotency key for live workflow admission.
        idempotency_key: Option<String>,
    },
}

impl ExperimentTarget {
    /// Returns the target kind discriminator for this payload.
    #[must_use]
    pub const fn kind(&self) -> ExperimentTargetKind {
        match self {
            Self::AgentLoop { .. } => ExperimentTargetKind::AgentLoop,
            Self::Workflow { .. } => ExperimentTargetKind::Workflow,
        }
    }
}

/// Variant under evaluation for a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentVariant {
    /// Human-readable variant name.
    pub name: String,
    /// Model selected for this variant.
    pub model: Option<ModelId>,
    /// Artifact revision identifiers included in this variant.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Skill artifact references included in this variant.
    pub skill_refs: Vec<String>,
    /// Workflow artifact reference included in this variant.
    pub workflow_ref: Option<String>,
    /// Variant-specific metadata.
    pub metadata: Value,
}

/// Scorecard definition attached to an experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScorecard {
    /// Score names expected for this experiment.
    pub score_names: Vec<String>,
    /// Metadata about evaluators that produce the scores.
    pub evaluator_metadata: Value,
}

/// Input used to create a durable experiment run row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewExperimentRun {
    /// Human-readable run name.
    pub name: String,
    /// Target payload used for execution.
    pub target: ExperimentTarget,
    /// Variant under evaluation.
    pub variant: ExperimentVariant,
    /// Scorecard attached to this experiment.
    pub scorecard: ExperimentScorecard,
    /// Score run identifier used to join against analytics scores.
    pub score_run_id: Uuid,
    /// Session linked to the experiment run, when one exists.
    pub session_id: Option<SessionId>,
    /// Workflow run linked to the experiment run, when one exists.
    pub workflow_run_uid: Option<Uuid>,
    /// Artifact revisions associated with this experiment run.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Optional idempotency key for scoped create deduplication.
    pub idempotency_key: Option<String>,
    /// Identity payload that accepted or created the experiment row.
    pub created_by_identity: Value,
}

/// Durable record for one experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunRecord {
    /// Memory-style scope that owns the experiment row.
    pub scope: MemoryScope,
    /// Stable experiment run identifier.
    pub run_uid: Uuid,
    /// Human-readable run name.
    pub name: String,
    /// Whether the run is a regression eval or live behavior experiment.
    pub run_kind: ExperimentRunKind,
    /// Fast discriminator for the target payload.
    pub target_kind: ExperimentTargetKind,
    /// Current run lifecycle status.
    pub status: ExperimentRunStatus,
    /// Target payload used for execution.
    pub target: ExperimentTarget,
    /// Variant under evaluation.
    pub variant: ExperimentVariant,
    /// Scorecard attached to this experiment.
    pub scorecard: ExperimentScorecard,
    /// Score run identifier used to join against analytics scores.
    pub score_run_id: Uuid,
    /// Session linked to the experiment run, when one exists.
    pub session_id: Option<SessionId>,
    /// Workflow run linked to the experiment run, when one exists.
    pub workflow_run_uid: Option<Uuid>,
    /// Artifact revisions associated with this experiment run.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Optional idempotency key for scoped create deduplication.
    pub idempotency_key: Option<String>,
    /// Identity payload that accepted or created the experiment row.
    pub created_by_identity: Value,
    /// Terminal error message for failed runs.
    pub error: Option<String>,
    /// Timestamp when the row was created and the run was accepted.
    pub created_at: DateTime<Utc>,
    /// Timestamp when execution started.
    pub started_at: Option<DateTime<Utc>>,
    /// Timestamp when execution reached a terminal state.
    pub completed_at: Option<DateTime<Utc>>,
    /// Timestamp when the record was last updated.
    pub updated_at: DateTime<Utc>,
}
