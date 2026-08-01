//! Skill export, listing, and package wire DTOs.

use chrono::{DateTime, Utc};
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Request payload for exporting tenant-visible skills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillExportRequest {
    /// Tenant whose visible skills should be exported.
    pub tenant_id: TenantId,
}

/// Response payload containing exported skill packages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillExportResponse {
    /// Tenant whose skills were exported.
    pub tenant_id: TenantId,
    /// Exported skill packages.
    #[serde(default)]
    pub packages: Vec<SkillPackageDocument>,
}

/// Skill package returned by export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillPackageDocument {
    /// Optional stable skill name parsed from `SKILL.md`.
    pub name: Option<String>,
    /// Optional one-line skill description parsed from `SKILL.md`.
    pub description: Option<String>,
    /// Files contained in this skill package.
    #[serde(default)]
    pub files: Vec<SkillPackageDocumentFile>,
    /// Optional logical source path or URI.
    pub source_uri: Option<String>,
    /// Additional skill metadata parsed by the server.
    #[serde(default)]
    pub metadata: Value,
}

/// One file in an exported skill package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillPackageDocumentFile {
    /// POSIX relative path inside the skill package.
    pub path: String,
    /// Base64-encoded file content.
    pub content_base64: String,
    /// Optional media type hint.
    pub content_type: Option<String>,
    /// Whether the file should be executable in a sandbox.
    #[serde(default)]
    pub executable: bool,
}

/// Request payload for listing skills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillListRequest {
    /// Tenant whose visible skills should be listed.
    pub tenant_id: TenantId,
}

/// Response payload containing listed skills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillListResponse {
    /// Listed skills ordered for API display.
    #[serde(default)]
    pub skills: Vec<SkillSummary>,
}

/// Summary of one visible skill version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillSummary {
    /// Stable row identifier for this skill version.
    pub skill_uid: Uuid,
    /// Scope where this skill is visible.
    pub scope: ActionRuleScope,
    /// Integer row-level skill version.
    pub version: i32,
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Tags associated with the skill.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Hex-encoded SHA-256 digest of the full package tree.
    pub package_hash: String,
    /// Hex-encoded SHA-256 digest of the required `SKILL.md`.
    pub skill_md_hash: String,
    /// Number of files in the package.
    pub file_count: i32,
    /// Total package size in bytes.
    pub total_size_bytes: i64,
    /// Timestamp when this skill version was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when this skill version was last updated.
    pub updated_at: DateTime<Utc>,
}
