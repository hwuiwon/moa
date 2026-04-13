//! Memory, wiki, and skill metadata types.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{UserId, WorkspaceId};

/// Scope for memory operations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// User-scoped memory.
    User(UserId),
    /// Workspace-scoped memory.
    Workspace(WorkspaceId),
}

string_id!(
    /// Logical memory wiki path.
    pub struct MemoryPath
);

/// Confidence level stored with wiki pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    /// High confidence.
    High,
    /// Medium confidence.
    Medium,
    /// Low confidence.
    Low,
}

/// Type of wiki page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageType {
    /// Index page such as `MEMORY.md`.
    Index,
    /// Topic page.
    Topic,
    /// Entity page.
    Entity,
    /// Decision page.
    Decision,
    /// Skill page.
    Skill,
    /// Source summary page.
    Source,
    /// Schema page.
    Schema,
    /// Log page.
    Log,
}

/// Result row returned from memory search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySearchResult {
    /// Scope that produced this search result.
    pub scope: MemoryScope,
    /// Logical page path.
    pub path: MemoryPath,
    /// Page title.
    pub title: String,
    /// Page type.
    pub page_type: PageType,
    /// Search snippet.
    pub snippet: String,
    /// Confidence level.
    pub confidence: ConfidenceLevel,
    /// Update timestamp.
    pub updated: DateTime<Utc>,
    /// Reference count.
    pub reference_count: u64,
}

/// Summary of a single source-ingest operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReport {
    /// Scope receiving the new source-derived pages.
    pub scope: MemoryScope,
    /// Human-readable source name passed by the caller.
    pub source_name: String,
    /// Summary page created for the raw source.
    pub source_path: MemoryPath,
    /// All pages created or updated by the ingest pass.
    pub affected_pages: Vec<MemoryPath>,
    /// Contradiction notes detected in the source text.
    pub contradictions: Vec<String>,
}

/// Compact wiki page listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSummary {
    /// Logical page path.
    pub path: MemoryPath,
    /// Page title.
    pub title: String,
    /// Page type.
    pub page_type: PageType,
    /// Confidence level.
    pub confidence: ConfidenceLevel,
    /// Update timestamp.
    pub updated: DateTime<Utc>,
}

/// Tier-1 skill metadata injected into the context pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Logical page path for the skill document.
    pub path: MemoryPath,
    /// Stable skill name from `SKILL.md`.
    pub name: String,
    /// Longer description from the Agent Skills frontmatter.
    pub description: String,
    /// User-defined tags.
    pub tags: Vec<String>,
    /// Tools referenced by the skill.
    pub allowed_tools: Vec<String>,
    /// Estimated token cost for the full skill body.
    pub estimated_tokens: usize,
    /// Historical usage count.
    pub use_count: u32,
    /// Historical success rate between `0.0` and `1.0`.
    pub success_rate: f32,
    /// Whether the skill was auto-generated.
    pub auto_generated: bool,
}

/// Full wiki page representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiPage {
    /// Logical page path.
    pub path: Option<MemoryPath>,
    /// Page title.
    pub title: String,
    /// Page type.
    pub page_type: PageType,
    /// Raw markdown body.
    pub content: String,
    /// Creation timestamp.
    pub created: DateTime<Utc>,
    /// Last update timestamp.
    pub updated: DateTime<Utc>,
    /// Confidence level.
    pub confidence: ConfidenceLevel,
    /// Explicit related links.
    pub related: Vec<String>,
    /// Provenance sources.
    pub sources: Vec<String>,
    /// User-defined tags.
    pub tags: Vec<String>,
    /// Whether the page was generated automatically.
    pub auto_generated: bool,
    /// Last referenced timestamp.
    pub last_referenced: DateTime<Utc>,
    /// Reference count.
    pub reference_count: u64,
    /// Arbitrary frontmatter fields preserved across parse and render round-trips.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}
