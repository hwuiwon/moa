//! Artifact service wire DTOs.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// One source/package file supplied with an artifact import or export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactFileDocument {
    /// POSIX relative path inside the artifact package.
    pub path: String,
    /// Base64-encoded file content.
    pub content_base64: String,
    /// Optional media type hint.
    pub content_type: Option<String>,
    /// Whether the file should be executable in a sandbox.
    #[serde(default)]
    pub executable: bool,
}

/// Request payload for importing a draft artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactImportRequest {
    /// Scope where the draft artifact should be written.
    pub scope: ActionRuleScope,
    /// Source format, currently `json` or `yaml`.
    pub source_format: String,
    /// Raw JSON or YAML artifact document.
    pub source_text: String,
    /// Optional package files stored with the artifact revision.
    #[serde(default)]
    pub files: Vec<ArtifactFileDocument>,
}

/// Response payload returned after importing a draft artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactImportResponse {
    /// Artifact row identifier.
    pub artifact_uid: Uuid,
    /// Draft revision row identifier.
    pub revision_uid: Uuid,
    /// Stored artifact status.
    pub status: String,
    /// Structured validation report for the draft.
    pub validation_report: Value,
}

/// Request payload for exporting a visible artifact revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactExportRequest {
    /// Tenant whose visible artifacts should be exported when no explicit scope is supplied.
    pub tenant_id: TenantId,
    /// Optional scope to read from, defaulting to the tenant tier.
    #[serde(default)]
    pub scope: Option<ActionRuleScope>,
    /// Artifact kind such as `skill`, `workflow`, or `experiment_plan`.
    pub kind: String,
    /// Artifact name.
    pub name: String,
    /// Optional source format preference, currently advisory.
    #[serde(default)]
    pub source_format: Option<String>,
}

/// Response payload containing an exported artifact revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactExportResponse {
    /// Artifact row identifier.
    pub artifact_uid: Uuid,
    /// Revision row identifier.
    pub revision_uid: Uuid,
    /// Artifact source format.
    pub source_format: String,
    /// Raw source text for this revision.
    pub source_text: String,
    /// Parsed artifact document as JSON.
    pub document: Value,
    /// Files stored with this artifact revision.
    #[serde(default)]
    pub files: Vec<ArtifactFileDocument>,
}

/// Request payload for listing visible artifacts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactListRequest {
    /// Tenant whose visible artifacts should be listed when no explicit scope is supplied.
    pub tenant_id: TenantId,
    /// Optional scope to list from, defaulting to the tenant tier.
    #[serde(default)]
    pub scope: Option<ActionRuleScope>,
    /// Optional artifact kind filter such as `skill`, `workflow`, or `experiment_plan`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional status filter.
    #[serde(default)]
    pub status: Option<String>,
}

/// Response payload containing visible artifact summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactListResponse {
    /// Listed artifact summaries.
    #[serde(default)]
    pub artifacts: Vec<ArtifactSummary>,
}

/// Summary of one visible artifact revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    /// Artifact row identifier.
    pub artifact_uid: Uuid,
    /// Revision row identifier.
    pub revision_uid: Uuid,
    /// Generated scope tier label.
    pub scope: String,
    /// Artifact kind.
    pub kind: String,
    /// Artifact name.
    pub name: String,
    /// Artifact description.
    pub description: String,
    /// Artifact tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Revision status.
    pub status: String,
    /// Revision version.
    pub version: i32,
    /// Timestamp when this revision was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Request payload for validating an artifact document without writing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactValidateRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Source format, currently `json` or `yaml`.
    pub source_format: String,
    /// Raw JSON or YAML artifact document.
    pub source_text: String,
    /// Desired lifecycle status for validation.
    #[serde(default)]
    pub status: Option<String>,
}

/// Response payload for artifact validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactValidateResponse {
    /// Whether validation produced no errors.
    pub valid: bool,
    /// Structured validation report.
    pub validation_report: Value,
}

/// Request payload for publishing a draft artifact revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactPublishRequest {
    /// Scope that owns the revision.
    pub scope: ActionRuleScope,
    /// Draft revision to publish.
    pub revision_uid: Uuid,
}

/// Response payload returned after publishing an artifact revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactPublishResponse {
    /// Artifact row identifier.
    pub artifact_uid: Uuid,
    /// Published revision row identifier.
    pub revision_uid: Uuid,
    /// Stored artifact status.
    pub status: String,
    /// Structured validation report used for publish.
    pub validation_report: Value,
}
