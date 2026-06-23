//! Action policy and tenant-admin review types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{SessionActorRef, SessionId, SubAgentId, TenantId, ToolCallId, UserId};

/// Risk level assigned to one policy-facing action.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RiskLevel {
    /// Low-risk read or metadata action.
    Low,
    /// Medium-risk action with bounded local side effects.
    Medium,
    /// High-risk action with external, destructive, privileged, or financial impact.
    High,
}

impl RiskLevel {
    /// Returns the stable database and metrics representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Policy/audit class for one action.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ActionClass {
    /// Reads local or remote data without side effects.
    Read,
    /// Writes to the local workspace or sandbox.
    LocalWrite,
    /// Runs a command in an execution environment.
    CommandExecution,
    /// Writes to an external service.
    ExternalWrite,
    /// Exports data outside the workspace boundary.
    DataExport,
    /// Deletes, overwrites, or otherwise destroys data.
    Destructive,
    /// Changes access, credentials, or permissions.
    PermissionChange,
    /// Deploys or changes live infrastructure.
    Deployment,
    /// Moves money or creates a billable financial side effect.
    MoneyMovement,
}

impl ActionClass {
    /// Returns the stable database and metrics representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Effect returned by action-policy evaluation.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ActionPolicyEffect {
    /// Execute the action.
    Allow,
    /// Reject the action without executing it.
    Deny,
    /// Queue the action for tenant-admin review.
    AdminReview,
}

impl ActionPolicyEffect {
    /// Returns the stable database and metrics representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Scope an action-policy rule applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRuleScope {
    /// Rule applies as the workspace-level inherited default.
    WorkspaceDefault,
    /// Rule overrides the workspace default inside one tenant.
    Tenant {
        /// Tenant that owns the override.
        tenant_id: TenantId,
    },
}

impl ActionRuleScope {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceDefault => "workspace_default",
            Self::Tenant { .. } => "tenant",
        }
    }
}

/// Result of evaluating action policy for one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPolicyDecision {
    /// Effect to apply.
    pub effect: ActionPolicyEffect,
    /// Optional human-readable reason for the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional policy rule that matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule_id: Option<Uuid>,
}

/// Persistent action-policy rule matched by tool and normalized input pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPolicyRule {
    /// Stable rule identifier.
    pub id: Uuid,
    /// Inheritance scope the rule applies to.
    pub scope: ActionRuleScope,
    /// Tool name this rule applies to.
    pub tool: String,
    /// Glob pattern used for matching normalized inputs.
    pub pattern: String,
    /// Effect to apply when the rule matches.
    pub effect: ActionPolicyEffect,
    /// Optional human-readable reason attached to the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// User who created the rule.
    pub created_by: UserId,
    /// Rule creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Durable policy-facing description of one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionEnvelope {
    /// Tenant-admin review identifier.
    pub review_id: Uuid,
    /// Tenant that owns the action.
    pub tenant_id: TenantId,
    /// Actor that requested the action.
    pub requested_by: SessionActorRef,
    /// Session that owns the action, when present.
    pub session_id: Option<SessionId>,
    /// Sub-agent that requested the action, when present.
    pub sub_agent_id: Option<SubAgentId>,
    /// Tool call identifier for the original model-visible request.
    pub tool_call_id: ToolCallId,
    /// Tool name being evaluated.
    pub tool_name: String,
    /// Normalized input used for policy matching.
    pub normalized_input: String,
    /// Concise human-readable input summary.
    pub input_summary: String,
    /// Risk level assigned by the tool definition and policy normalizer.
    pub risk_level: RiskLevel,
    /// Policy/audit class assigned to the action.
    pub action_class: ActionClass,
    /// Origin object kind for workflow or artifact-driven actions.
    pub origin_kind: Option<String>,
    /// Origin object identifier for workflow or artifact-driven actions.
    pub origin_id: Option<String>,
    /// Origin step identifier for workflow or artifact-driven actions.
    pub origin_step_id: Option<String>,
    /// Explicit idempotency key supplied for side-effecting tools.
    pub idempotency_key: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Human-readable action-review preview rendered to tenant admins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewPreview {
    /// Structured fields rendered by review surfaces.
    pub fields: Vec<ActionReviewField>,
    /// File diffs rendered inline and in full-screen diff viewers.
    pub file_diffs: Vec<ActionReviewFileDiff>,
}

/// Human-readable action-review field shown in review surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewField {
    /// Field label.
    pub label: String,
    /// Human-readable value.
    pub value: String,
}

/// A text file diff attached to an action review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewFileDiff {
    /// Logical file path shown to the reviewer.
    pub path: String,
    /// Existing file contents before the tool executes.
    pub before: String,
    /// Proposed file contents after the tool executes.
    pub after: String,
    /// Optional syntax hint derived from the file extension.
    pub language_hint: Option<String>,
}

/// Workspace-admin decision for one action review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionReviewDecision {
    /// The action was cleared for later execution.
    Cleared,
    /// The action was denied by a tenant admin.
    Denied {
        /// Optional human-readable denial reason.
        reason: Option<String>,
    },
}

/// Current status of a tenant action review.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ActionReviewStatus {
    /// Review is waiting for a workspace-admin decision.
    Pending,
    /// Review was cleared.
    Cleared,
    /// Review was denied.
    Denied,
}

impl ActionReviewStatus {
    /// Returns the stable database and metrics representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}
