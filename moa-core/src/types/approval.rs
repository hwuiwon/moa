//! Approval flow types and policy metadata.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{UserId, WorkspaceId};

/// Risk level for approval decisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Low-risk action.
    Low,
    /// Medium-risk action.
    Medium,
    /// High-risk action.
    High,
}

/// Approval decision returned by a user.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow exactly once.
    AllowOnce,
    /// Persist an allow rule.
    AlwaysAllow { pattern: String },
    /// Deny the request.
    Deny { reason: Option<String> },
}

/// Approval request details rendered to a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Approval request identifier.
    pub request_id: Uuid,
    /// Tool name being approved.
    pub tool_name: String,
    /// Human-readable input summary.
    pub input_summary: String,
    /// Risk level assigned to the request.
    pub risk_level: RiskLevel,
}

/// Human-readable approval field shown in local UI surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalField {
    /// Field label.
    pub label: String,
    /// Human-readable value.
    pub value: String,
}

/// A text file diff attached to a pending approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalFileDiff {
    /// Logical file path shown to the user.
    pub path: String,
    /// Existing file contents before the tool executes.
    pub before: String,
    /// Proposed file contents after the tool executes.
    pub after: String,
    /// Optional syntax hint derived from the file extension.
    pub language_hint: Option<String>,
}

/// Approval prompt emitted by the local orchestrator runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    /// Approval request displayed to the user.
    pub request: ApprovalRequest,
    /// Suggested rule pattern when the user chooses "Always Allow".
    pub pattern: String,
    /// Structured parameters rendered by the approval widget.
    pub parameters: Vec<ApprovalField>,
    /// Optional file diffs rendered inline and in the full-screen diff viewer.
    pub file_diffs: Vec<ApprovalFileDiff>,
}

/// Persistent approval rule action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// Automatically allow matching tool calls.
    Allow,
    /// Automatically deny matching tool calls.
    Deny,
    /// Require an explicit human approval.
    RequireApproval,
}

/// Scope a persistent approval rule applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyScope {
    /// Rule applies within a single workspace.
    Workspace,
    /// Rule applies globally across workspaces.
    Global,
}

/// Persistent approval rule stored for tool execution policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRule {
    /// Stable rule identifier.
    pub id: Uuid,
    /// Workspace the rule belongs to.
    pub workspace_id: WorkspaceId,
    /// Tool name this rule applies to.
    pub tool: String,
    /// Glob pattern used for matching normalized inputs.
    pub pattern: String,
    /// Action to take when the rule matches.
    pub action: PolicyAction,
    /// Scope the rule applies to.
    pub scope: PolicyScope,
    /// User who created the rule.
    pub created_by: UserId,
    /// Rule creation timestamp.
    pub created_at: DateTime<Utc>,
}
