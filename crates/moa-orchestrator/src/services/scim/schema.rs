//! SCIM resource shapes per RFC 7643.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// SCIM User resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScimUser {
    /// Resource schema URNs.
    pub schemas: Vec<String>,
    /// MOA user id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// IdP-provided stable external id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Login name, represented as email in MOA.
    pub user_name: String,
    /// Structured name fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<Name>,
    /// Display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Email addresses.
    #[serde(default)]
    pub emails: Vec<ScimEmail>,
    /// Whether the user is active.
    pub active: bool,
    /// Resource metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// SCIM User name fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Name {
    /// Given name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    /// Family name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    /// Formatted display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
}

/// SCIM email value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScimEmail {
    /// Email address.
    pub value: String,
    /// Primary email marker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<bool>,
    /// Email type, such as `work`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// SCIM Group resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScimGroup {
    /// Resource schema URNs.
    pub schemas: Vec<String>,
    /// MOA group id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// IdP-provided stable external id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Group display name.
    pub display_name: String,
    /// Group members.
    #[serde(default)]
    pub members: Vec<ScimGroupMember>,
    /// Resource metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// SCIM group member reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimGroupMember {
    /// User id.
    pub value: String,
    /// Optional display label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

/// SCIM resource metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    /// Resource type, `User` or `Group`.
    pub resource_type: String,
    /// Creation timestamp.
    pub created: DateTime<Utc>,
    /// Last mutation timestamp.
    pub last_modified: DateTime<Utc>,
    /// Weak ETag derived from the row version.
    pub version: String,
    /// Resource URL.
    pub location: String,
}

/// SCIM list response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResponse<T> {
    /// Response schema URNs.
    pub schemas: Vec<String>,
    /// Total result count before pagination.
    #[serde(rename = "totalResults")]
    pub total_results: i64,
    /// Number of items in this page.
    #[serde(rename = "itemsPerPage")]
    pub items_per_page: i64,
    /// One-based start index.
    #[serde(rename = "startIndex")]
    pub start_index: i64,
    /// Returned resources.
    #[serde(rename = "Resources")]
    pub resources: Vec<T>,
}

/// SCIM error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScimError {
    /// Error schema URNs.
    pub schemas: Vec<String>,
    /// HTTP status code as a string, per SCIM.
    pub status: String,
    /// Human-readable error detail.
    pub detail: String,
    /// Optional SCIM error type.
    #[serde(rename = "scimType", skip_serializing_if = "Option::is_none")]
    pub scim_type: Option<String>,
}

/// SCIM User schema URN.
pub const SCHEMA_USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
/// SCIM Group schema URN.
pub const SCHEMA_GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
/// SCIM ListResponse schema URN.
pub const SCHEMA_LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
/// SCIM Error schema URN.
pub const SCHEMA_ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
/// SCIM PatchOp schema URN.
pub const SCHEMA_PATCH: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
