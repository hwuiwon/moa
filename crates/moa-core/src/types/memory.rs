//! Skill metadata types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Tier-1 skill metadata injected into the context pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Exact artifact revision backing this skill metadata, when loaded from artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_revision_uid: Option<uuid::Uuid>,
    /// Canonical skill document path.
    pub path: String,
    /// Stable skill name from `SKILL.md`.
    pub name: String,
    /// Longer description from the Agent Skills frontmatter.
    pub description: String,
    /// User-defined tags.
    pub tags: Vec<String>,
    /// Tools referenced by the skill.
    pub allowed_tools: Vec<String>,
    /// Callable action names exposed by the skill artifact, if any.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Estimated token cost for the full skill body.
    pub estimated_tokens: usize,
    /// Historical usage count.
    pub use_count: u32,
    /// Last time the skill was used, when tracked in metadata.
    pub last_used: Option<DateTime<Utc>>,
    /// Historical success rate between `0.0` and `1.0`.
    pub success_rate: f32,
    /// Whether the skill was auto-generated.
    pub auto_generated: bool,
}
