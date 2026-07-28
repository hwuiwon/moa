//! Knowledge object, parsed document, block, chunk, and inspection types.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{KnowledgeIngestionStep, ObjectAcl};

/// Source-side object such as a file, page, ticket, message, or CRM record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObject {
    /// Tenant-owned object identifier.
    pub object_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning connection.
    pub connection_uid: Uuid,
    /// Source object type.
    pub object_type: String,
    /// Provider source identifier.
    pub source_id: String,
    /// Optional stable parent object identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_source_id: Option<String>,
    /// Source URI when safe to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Renderer-safe title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Provider change token or etag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_token: Option<String>,
    /// Safe source metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Current ingestion status.
    pub status: ObjectStatus,
    /// Provider ACL position of this object.
    ///
    /// Required, with no serde default: an object that reaches retrieval without
    /// a recorded ACL position would be indistinguishable from one whose
    /// permissions were never captured, and the safe reading of that is not a
    /// reading anyone should have to infer.
    pub acl: ObjectAcl,
    /// Source update time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_updated_at: Option<DateTime<Utc>>,
    /// Soft deletion timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Knowledge object status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectStatus {
    /// Object was observed but not ingested.
    Pending,
    /// Object has active parsed content.
    Active,
    /// Object was deleted at the provider.
    Deleted,
    /// Object failed ingestion.
    Error,
}

impl ObjectStatus {
    /// Returns the stable database status identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Deleted => "deleted",
            Self::Error => "error",
        }
    }
}

/// One immutable parsed content version for a source object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentVersion {
    /// Version identifier.
    pub version_uid: Uuid,
    /// Owning object.
    pub object_uid: Uuid,
    /// Parser identifier.
    pub parser: String,
    /// Parser job identifier when supplied by an external parser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser_job_id: Option<String>,
    /// Content hash for this version.
    pub content_hash: String,
    /// Parser metadata safe for inspection.
    #[serde(default)]
    pub metadata: Value,
    /// Version creation timestamp.
    pub created_at: DateTime<Utc>,
}

impl DocumentVersion {
    /// Returns the graph node uid this version materializes.
    ///
    /// A knowledge `Document` node carries the object's title and source id, so
    /// it is source-governed content in its own right and needs the same ACL
    /// admission as the version's chunks. The uid is a pure function of the
    /// version id — the same derivation the graph delta uses — so the stored
    /// column is a lookup index for admission, never a second source of truth.
    #[must_use]
    pub fn graph_node_uid(&self) -> Uuid {
        Self::graph_node_uid_for(self.version_uid)
    }

    /// Returns the graph node uid for one document version id.
    #[must_use]
    pub fn graph_node_uid_for(version_uid: Uuid) -> Uuid {
        crate::graph_delta::stable_uid(&format!("document:{version_uid}"))
    }
}

/// Input accepted by a document parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParseInput {
    /// Object being parsed.
    pub object: KnowledgeObject,
    /// Optional file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Optional MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional source URL or presigned URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Optional raw bytes for local parsing or upload-style APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
    /// Optional already-normalized text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Parser options.
    #[serde(default)]
    pub options: Value,
}

/// Parser output normalized across native and external parsers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedDocument {
    /// Parser identifier.
    pub parser: String,
    /// Parser job identifier when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser_job_id: Option<String>,
    /// Rendered full text or markdown.
    pub text: String,
    /// Structured document elements.
    #[serde(default)]
    pub elements: Vec<DocumentElement>,
    /// Safe parser metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Parser output unit such as heading, paragraph, table, field, or block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentElement {
    /// Stable parser element identifier.
    pub element_id: String,
    /// Normalized element kind.
    pub kind: DocumentElementKind,
    /// Element text.
    pub text: String,
    /// Heading path active for this element.
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Document-order ordinal.
    pub ordinal: u32,
    /// 1-based page number when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_number: Option<u32>,
    /// Layout metadata when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<ElementLayout>,
    /// Safe parser-specific metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Normalized parser element kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentElementKind {
    /// Heading or title.
    Heading,
    /// Paragraph or narrative text.
    Paragraph,
    /// List item.
    ListItem,
    /// Table or table row.
    Table,
    /// Message-like source object.
    Message,
    /// Field or record attribute.
    Field,
    /// Attachment reference.
    Attachment,
    /// Figure or caption.
    Figure,
    /// Page boundary or page-level text.
    Page,
    /// Parser chunk structure.
    ParserChunk,
    /// Unknown but text-bearing element.
    Other,
}

/// Layout metadata for visual citations.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ElementLayout {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
    /// Page width when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_width: Option<f32>,
    /// Page height when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_height: Option<f32>,
    /// OCR or parser confidence when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Normalized atomic knowledge unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeBlock {
    /// Block identifier.
    pub block_uid: Uuid,
    /// Owning document version.
    pub version_uid: Uuid,
    /// Source element identifier.
    pub element_id: String,
    /// Deterministic block content hash.
    pub block_hash: String,
    /// Normalized text used for hashing and chunking.
    pub normalized_text: String,
    /// Heading path used for citation rendering.
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Document-order ordinal.
    pub ordinal: u32,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Retrieval-sized group of consecutive knowledge blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeChunk {
    /// Chunk occurrence identifier, and the graph node UID for this occurrence.
    ///
    /// One chunk row is one graph occurrence: `knowledge_chunks.graph_node_uid`
    /// is stored equal to this value and the database enforces the equality, so
    /// equal text in two documents (or in two versions of one document) never
    /// collapses onto one graph node, embedding, citation, or deletion target.
    pub chunk_uid: Uuid,
    /// Owning document version.
    pub version_uid: Uuid,
    /// Deterministic chunk content hash.
    pub chunk_hash: String,
    /// Ordered source block hashes.
    pub block_hashes: Vec<String>,
    /// Chunk text.
    pub text: String,
    /// Heading path for rendering.
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Chunk ordinal.
    pub ordinal: u32,
    /// Approximate token count.
    pub token_count: usize,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Source object plus parser and graph counters for service projections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectProjection {
    /// Source object.
    pub object: KnowledgeObject,
    /// Latest parser that produced content for the object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser: Option<String>,
    /// Current parser status for the object.
    pub parser_status: String,
    /// Current chunk count.
    ///
    /// Every chunk is its own graph occurrence, so this is also the object's
    /// current graph chunk-node count; there is no separate counter that could
    /// disagree.
    pub chunk_count: u64,
}

/// Object inspection projection assembled from object, version, chunks, and steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectInspection {
    /// Source object.
    pub object: KnowledgeObject,
    /// Latest parsed document version, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<DocumentVersion>,
    /// Current chunks for the latest version.
    #[serde(default)]
    pub chunks: Vec<KnowledgeChunk>,
    /// Ordered object ingestion timeline.
    #[serde(default)]
    pub steps: Vec<KnowledgeIngestionStep>,
}
